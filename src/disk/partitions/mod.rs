mod block_info;
mod builder;
mod limitations;
mod os_detect;

use self::block_info::BlockInfo;
pub use self::builder::PartitionBuilder;
pub use self::limitations::check_partition_size;
use self::os_detect::detect_os;
use super::{get_uuid, LvmEncryption, Mounts, PartitionSizeError, Swaps};
use super::usage::get_used_sectors;
use libparted::{Partition, PartitionFlag};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Specifies which file system format to use.
#[derive(Debug, PartialEq, Copy, Clone, Hash)]
pub enum FileSystemType {
    Btrfs,
    Exfat,
    Ext2,
    Ext3,
    Ext4,
    F2fs,
    Fat16,
    Fat32,
    Ntfs,
    Swap,
    Xfs,
    Lvm,
}

impl FileSystemType {
    fn get_preferred_options(&self) -> &'static str {
        match *self {
            FileSystemType::Fat16 | FileSystemType::Fat32 => "umask=0077",
            FileSystemType::Ext4 => "noatime,errors=remount-ro",
            FileSystemType::Swap => "sw",
            _ => "defaults",
        }
    }
}

impl FromStr for FileSystemType {
    type Err = &'static str;
    fn from_str(string: &str) -> Result<Self, Self::Err> {
        let type_ = match string {
            "btrfs" => FileSystemType::Btrfs,
            "exfat" => FileSystemType::Exfat,
            "ext2" => FileSystemType::Ext2,
            "ext3" => FileSystemType::Ext3,
            "ext4" => FileSystemType::Ext4,
            "f2fs" => FileSystemType::F2fs,
            "fat16" => FileSystemType::Fat16,
            "fat32" => FileSystemType::Fat32,
            "swap" | "linux-swap(v1)" => FileSystemType::Swap,
            "ntfs" => FileSystemType::Ntfs,
            "xfs" => FileSystemType::Xfs,
            "lvm" => FileSystemType::Lvm,
            _ => return Err("invalid file system name"),
        };
        Ok(type_)
    }
}

impl Into<&'static str> for FileSystemType {
    fn into(self) -> &'static str {
        match self {
            FileSystemType::Btrfs => "btrfs",
            FileSystemType::Exfat => "exfat",
            FileSystemType::Ext2 => "ext2",
            FileSystemType::Ext3 => "ext3",
            FileSystemType::Ext4 => "ext4",
            FileSystemType::F2fs => "f2fs",
            FileSystemType::Fat16 => "fat16",
            FileSystemType::Fat32 => "fat32",
            FileSystemType::Ntfs => "ntfs",
            FileSystemType::Swap => "linux-swap(v1)",
            FileSystemType::Xfs => "xfs",
            FileSystemType::Lvm => "lvm",
        }
    }
}

/// Defines whether the partition is a primary or logical partition.
#[derive(Debug, PartialEq, Clone, Copy, Hash)]
pub enum PartitionType {
    Primary,
    Logical,
}

// TODO: Compress boolean fields into a single byte.

/// Contains relevant information about a certain partition.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionInfo {
    /// If set to true, this is a source partition, which means it currently exists on the
    /// disk.
    pub(crate) is_source: bool,
    /// Source partitions will set this field. If set, this partition will be removed.
    pub(crate) remove: bool,
    /// Whether the filesystem should be formatted or not.
    pub format: bool,
    /// If the partition is currently active, this will be true.
    pub active: bool,
    /// If the partition is currently busy, this will be true.
    pub busy: bool,
    /// The partition number is the numeric value that follows the disk's device path.
    /// IE: _/dev/sda1_
    pub number: i32,
    /// The initial sector where the partition currently, or will, reside.
    pub start_sector: u64,
    /// The final sector where the partition currently, or will, reside.
    /// # Note
    /// The length of the partion can be calculated by substracting the `end_sector`
    /// from the `start_sector`, and multiplying that by the value of the disk's
    /// sector size.
    pub end_sector: u64,
    /// Whether this partition is a primary or logical partition.
    pub part_type: PartitionType,
    /// Whether there is a file system currently, or will be, on this partition.
    pub filesystem: Option<FileSystemType>,
    /// Specifies optional flags that should be applied to the partition, if not already set.
    pub flags: Vec<PartitionFlag>,
    /// Specifies the name of the partition.
    pub name: Option<String>,
    /// Contains the device path of the partition, which is the disk's device path plus
    /// the partition number.
    pub device_path: PathBuf,
    /// Where this partition is mounted in the file system, if at all.
    pub mount_point: Option<PathBuf>,
    /// True if the partition is currently used for swap
    pub swapped: bool,
    /// Where this partition will be mounted in the future
    pub target: Option<PathBuf>,
    /// The volume group associated with this device.
    pub volume_group: Option<(String, Option<LvmEncryption>)>,
    /// If the partition is associated with a keyfile, this will name the key and it's mount
    /// path.
    pub key_id: Option<(String, PathBuf)>,
}

impl PartitionInfo {
    pub fn new_from_ped(
        partition: &Partition,
        is_msdos: bool,
    ) -> io::Result<Option<PartitionInfo>> {
        let device_path = partition.get_path().unwrap().to_path_buf();
        info!(
            "libdistinst: obtaining partition information from {}",
            device_path.display()
        );
        let mounts = Mounts::new()?;
        let swaps = Swaps::new()?;

        Ok(Some(PartitionInfo {
            is_source: true,
            remove: false,
            format: false,
            part_type: match partition.type_get_name() {
                "primary" => PartitionType::Primary,
                "logical" => PartitionType::Logical,
                _ => return Ok(None),
            },
            mount_point: mounts.get_mount_point(&device_path),
            swapped: swaps.get_swapped(&device_path),
            target: None,
            filesystem: partition
                .fs_type_name()
                .and_then(|name| FileSystemType::from_str(name).ok()),
            flags: get_flags(partition),
            number: partition.num(),
            name: if is_msdos {
                None
            } else {
                partition.name().map(String::from)
            },
            // Note that primary and logical partitions should always have a path.
            device_path,
            active: partition.is_active(),
            busy: partition.is_busy(),
            start_sector: partition.geom_start() as u64,
            end_sector: partition.geom_end() as u64,
            // TODO: detect if this is assigned to a volume group:
            //       pvdisplay $PATH
            volume_group: None,
            key_id: None,
        }))
    }

    /// Assigns the partition to a key ID, and defines where it should mounted
    /// within the "/etc/crypttab" file.
    pub fn set_keydata(&mut self, id: String, target: PathBuf) {
        self.key_id = Some((id, target));
        self.target = None;
    }

    /// Returns the length of the partition in sectors.
    pub fn sectors(&self) -> u64 { self.end_sector - self.start_sector }

    /// Returns true if the partition is a swap partition.
    pub fn is_swap(&self) -> bool {
        self.filesystem
            .clone()
            .map_or(false, |fs| fs == FileSystemType::Swap)
    }

    /// Returns the path to this device in the system.
    pub fn get_device_path(&self) -> &Path { &self.device_path }

    pub(crate) fn requires_changes(&self, other: &PartitionInfo) -> bool {
        self.sectors_differ_from(other) || self.filesystem != other.filesystem || other.format
    }

    pub(crate) fn sectors_differ_from(&self, other: &PartitionInfo) -> bool {
        self.start_sector != other.start_sector || self.end_sector != other.end_sector
    }

    pub(crate) fn is_same_partition_as(&self, other: &PartitionInfo) -> bool {
        self.is_source && other.is_source && self.number == other.number
    }

    /// Defines a mount target for this partition.
    pub fn set_mount(&mut self, target: PathBuf) { self.target = Some(target); }

    /// Defines that the partition belongs to a given volume group.
    ///
    /// Optionally, this partition may be encrypted, in which you will also need to
    /// specify a new physical volume name as well. In the event of encryption, an LVM
    /// device will be assigned to the encrypted partition.
    pub fn set_volume_group(&mut self, group: String, encryption: Option<LvmEncryption>) {
        self.volume_group = Some((group.clone(), encryption));
    }

    /// Defines that a new file system will be applied to this partition.
    pub fn format_with(&mut self, fs: FileSystemType) {
        self.format = true;
        self.filesystem = Some(fs);
        self.name = None;
    }

    /// Returns the number of used sectors on the file system that belongs to this partition.
    pub fn sectors_used(&self, sector_size: u64) -> Option<io::Result<u64>> {
        use FileSystemType::*;
        self.filesystem.and_then(|fs| match fs {
            Swap | Lvm | Xfs | F2fs | Exfat => None,
            _ => Some(get_used_sectors(self.get_device_path(), fs, sector_size)),
        })
    }

    /// Detects if an OS is installed to this partition, and if so, what the OS is named.
    pub fn probe_os(&self) -> Option<String> {
        self.filesystem
            .and_then(|fs| detect_os(self.get_device_path(), fs))
    }

    /// Specifies to delete this partition from the partition table.
    pub fn remove(&mut self) { self.remove = true; }

    /// Obtains bock information for the partition, if possible, for use with
    /// generating entries in "/etc/fstab".
    pub(crate) fn get_block_info(&self) -> Option<BlockInfo> {
        info!(
            "libdistinst: getting block information for partition at {}",
            self.device_path.display()
        );

        if self.filesystem != Some(FileSystemType::Swap)
            && (self.target.is_none() || self.filesystem.is_none())
        {
            return None;
        }

        let result = get_uuid(&self.device_path).map(|uuid| {
            let fs = self.filesystem.clone().unwrap();
            BlockInfo {
                uuid,
                mount: if fs == FileSystemType::Swap {
                    None
                } else {
                    Some(self.target.clone().unwrap())
                },
                fs: match fs {
                    FileSystemType::Fat16 | FileSystemType::Fat32 => "vfat",
                    FileSystemType::Swap => "swap",
                    _ => fs.clone().into(),
                },
                options: fs.get_preferred_options().into(),
                dump: false,
                pass: false,
            }
        });

        if !result.is_some() {
            error!(
                "{}: no UUID associated with device",
                self.device_path.display()
            );
        }

        result
    }
}

const FLAGS: &[PartitionFlag] = &[
    PartitionFlag::PED_PARTITION_BOOT,
    PartitionFlag::PED_PARTITION_ROOT,
    PartitionFlag::PED_PARTITION_SWAP,
    PartitionFlag::PED_PARTITION_HIDDEN,
    PartitionFlag::PED_PARTITION_RAID,
    PartitionFlag::PED_PARTITION_LVM,
    PartitionFlag::PED_PARTITION_LBA,
    PartitionFlag::PED_PARTITION_HPSERVICE,
    PartitionFlag::PED_PARTITION_PALO,
    PartitionFlag::PED_PARTITION_PREP,
    PartitionFlag::PED_PARTITION_MSFT_RESERVED,
    PartitionFlag::PED_PARTITION_BIOS_GRUB,
    PartitionFlag::PED_PARTITION_APPLE_TV_RECOVERY,
    PartitionFlag::PED_PARTITION_DIAG,
    PartitionFlag::PED_PARTITION_LEGACY_BOOT,
    PartitionFlag::PED_PARTITION_MSFT_DATA,
    PartitionFlag::PED_PARTITION_IRST,
    PartitionFlag::PED_PARTITION_ESP,
];

fn get_flags(partition: &Partition) -> Vec<PartitionFlag> {
    FLAGS
        .into_iter()
        .filter(|&&f| partition.is_flag_available(f) && partition.get_flag(f))
        .cloned()
        .collect::<Vec<PartitionFlag>>()
}