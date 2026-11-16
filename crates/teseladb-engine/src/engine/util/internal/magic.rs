//! Filesystem magic numbers.
//!
//! These are the `f_type` values that `statfs(2)` reports on Linux. The file layer uses
//! them to pick per-filesystem I/O strategies (for example, the alignment and append
//! behaviour that is safe on XFS versus ext4 or tmpfs).

/// `f_type` of XFS.
pub const XFS_SUPER_MAGIC: u64 = 0x58465342;
/// `f_type` of ext2 (shared by ext3 and ext4).
pub const EXT2_SUPER_MAGIC: u64 = 0xEF53;
/// `f_type` of ext3 (same value as ext2).
pub const EXT3_SUPER_MAGIC: u64 = 0xEF53;
/// `f_type` of ext4 (same value as ext2).
pub const EXT4_SUPER_MAGIC: u64 = 0xEF53;
/// `f_type` of NFS.
pub const NFS_SUPER_MAGIC: u64 = 0x6969;
/// `f_type` of tmpfs.
pub const TMPFS_MAGIC: u64 = 0x01021994;
/// `f_type` of FUSE filesystems.
pub const FUSE_SUPER_MAGIC: u64 = 0x65735546;
/// `f_type` of Btrfs.
pub const BTRFS_SUPER_MAGIC: u64 = 0x9123683E;
/// `f_type` of HFS.
pub const HFS_SUPER_MAGIC: u64 = 0x4244;

/// Grouped access to the magic numbers; not part of the stable API.
pub mod internal {
    use super::*;

    /// Namespace for the filesystem magic numbers, as associated constants.
    pub struct FsMagic;

    impl FsMagic {
        /// See [`XFS_SUPER_MAGIC`].
        pub const XFS: u64 = XFS_SUPER_MAGIC;
        /// See [`EXT2_SUPER_MAGIC`].
        pub const EXT2: u64 = EXT2_SUPER_MAGIC;
        /// See [`EXT3_SUPER_MAGIC`].
        pub const EXT3: u64 = EXT3_SUPER_MAGIC;
        /// See [`EXT4_SUPER_MAGIC`].
        pub const EXT4: u64 = EXT4_SUPER_MAGIC;
        /// See [`NFS_SUPER_MAGIC`].
        pub const NFS: u64 = NFS_SUPER_MAGIC;
        /// See [`TMPFS_MAGIC`].
        pub const TMPFS: u64 = TMPFS_MAGIC;
        /// See [`FUSE_SUPER_MAGIC`].
        pub const FUSE: u64 = FUSE_SUPER_MAGIC;
        /// See [`BTRFS_SUPER_MAGIC`].
        pub const BTRFS: u64 = BTRFS_SUPER_MAGIC;
        /// See [`HFS_SUPER_MAGIC`].
        pub const HFS: u64 = HFS_SUPER_MAGIC;
    }
}
