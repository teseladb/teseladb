//! Flag and enumeration types used by the file API.
//!
//! [`OpenFlags`] selects how a file is opened (the Linux `O_*` values), [`AccessFlags`]
//! and [`FilePermissions`] are the `access(2)` modes and the mode bits of new files and
//! directories, [`DirectoryEntryType`] classifies listed entries and [`FsType`] names the
//! filesystem a file lives on. The flag types are thin newtypes over `i32` combined with
//! `|` and `&`.

/// Implements `|`, `|=`, `&`, `&=` over the underlying integer, plus `bits`/`from_bits`.
macro_rules! flag_ops {
    ($t:ident) => {
        impl std::ops::BitOr for $t {
            type Output = $t;
            #[inline]
            fn bitor(self, b: $t) -> $t {
                $t(self.0 | b.0)
            }
        }
        impl std::ops::BitOrAssign for $t {
            #[inline]
            fn bitor_assign(&mut self, b: $t) {
                self.0 |= b.0;
            }
        }
        impl std::ops::BitAnd for $t {
            type Output = $t;
            #[inline]
            fn bitand(self, b: $t) -> $t {
                $t(self.0 & b.0)
            }
        }
        impl std::ops::BitAndAssign for $t {
            #[inline]
            fn bitand_assign(&mut self, b: $t) {
                self.0 &= b.0;
            }
        }
        impl $t {
            /// The underlying integer bits.
            #[inline]
            pub const fn bits(self) -> i32 {
                self.0
            }
            /// The value with the given underlying bits.
            #[inline]
            pub const fn from_bits(bits: i32) -> Self {
                $t(bits)
            }
        }
    };
}

/// How a file is to be opened: the Linux `open(2)` flags, combined with `|`.
///
/// See [`crate::engine::core::file`] for the functions that take it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpenFlags(i32);

impl OpenFlags {
    /// Open for reading and writing (`O_RDWR`).
    pub const RW: OpenFlags = OpenFlags(0o2);
    /// Open for reading only (`O_RDONLY`).
    pub const RO: OpenFlags = OpenFlags(0);
    /// Open for writing only (`O_WRONLY`).
    pub const WO: OpenFlags = OpenFlags(0o1);
    /// Create the file if it does not exist (`O_CREAT`).
    pub const CREATE: OpenFlags = OpenFlags(0o100);
    /// Truncate an existing file to length zero (`O_TRUNC`).
    pub const TRUNCATE: OpenFlags = OpenFlags(0o1000);
    /// With [`OpenFlags::CREATE`], fail if the file already exists (`O_EXCL`).
    pub const EXCLUSIVE: OpenFlags = OpenFlags(0o200);
    /// Each write completes only once its data is durable (`O_DSYNC`).
    pub const DSYNC: OpenFlags = OpenFlags(0o10000);
}

flag_ops!(OpenFlags);

/// The type of a directory entry returned when listing a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DirectoryEntryType {
    /// The filesystem did not report the type.
    Unknown,
    /// A block device.
    BlockDevice,
    /// A character device.
    CharDevice,
    /// A directory.
    Directory,
    /// A named pipe.
    Fifo,
    /// A symbolic link.
    Link,
    /// A regular file.
    Regular,
    /// A Unix domain socket.
    Socket,
}

/// Kernel ABI definitions used by directory listing.
pub mod internal {
    /// Linux kernel structures read directly from system call results.
    pub mod linux_abi {
        // The getdents64(2) record uses 64-bit inode numbers and offsets.
        const _: () = assert!(std::mem::size_of::<u64>() == 8, "large file support not enabled");
        const _: () = assert!(std::mem::size_of::<i64>() == 8, "large file support not enabled");

        /// One record of the buffer filled by `getdents64(2)`; records are variable
        /// length and `d_reclen` gives the distance to the next one.
        #[repr(C)]
        #[derive(Debug)]
        pub struct LinuxDirent64 {
            /// 64-bit inode number.
            pub d_ino: u64,
            /// Offset of the next record in the directory stream.
            pub d_off: i64,
            /// Size of this record in bytes.
            pub d_reclen: u16,
            /// File type (`DT_*`).
            pub d_type: u8,
            /// Start of the null-terminated file name, which extends past the struct.
            pub d_name: [u8; 0],
        }
    }
}

/// The type of the filesystem a file lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsType {
    /// A filesystem not listed here.
    Other,
    /// XFS.
    Xfs,
    /// ext2.
    Ext2,
    /// ext3.
    Ext3,
    /// ext4.
    Ext4,
    /// Btrfs.
    Btrfs,
    /// HFS.
    Hfs,
    /// tmpfs (memory-backed).
    Tmpfs,
}

/// Access modes checked by `access(2)` on files and directories, combined with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccessFlags(i32);

impl AccessFlags {
    /// The path exists (`F_OK`).
    pub const EXISTS: AccessFlags = AccessFlags(0);
    /// Readable (`R_OK`).
    pub const READ: AccessFlags = AccessFlags(4);
    /// Writable (`W_OK`).
    pub const WRITE: AccessFlags = AccessFlags(2);
    /// Executable (`X_OK`).
    pub const EXECUTE: AccessFlags = AccessFlags(1);
    /// Searchable directory: the same bit as [`AccessFlags::EXECUTE`].
    pub const LOOKUP: AccessFlags = Self::EXECUTE;
}

impl std::ops::BitOr for AccessFlags {
    type Output = AccessFlags;
    #[inline]
    fn bitor(self, b: AccessFlags) -> AccessFlags {
        AccessFlags(self.0 | b.0)
    }
}

impl std::ops::BitAnd for AccessFlags {
    type Output = AccessFlags;
    #[inline]
    fn bitand(self, b: AccessFlags) -> AccessFlags {
        AccessFlags(self.0 & b.0)
    }
}

impl AccessFlags {
    /// The underlying integer bits.
    #[inline]
    pub const fn bits(self) -> i32 {
        self.0
    }
}

/// Mode bits for new files and directories, combined with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FilePermissions(i32);

impl FilePermissions {
    /// Read by owner.
    pub const USER_READ: FilePermissions = FilePermissions(0o400);
    /// Write by owner.
    pub const USER_WRITE: FilePermissions = FilePermissions(0o200);
    /// Execute by owner.
    pub const USER_EXECUTE: FilePermissions = FilePermissions(0o100);

    /// Read by group.
    pub const GROUP_READ: FilePermissions = FilePermissions(0o040);
    /// Write by group.
    pub const GROUP_WRITE: FilePermissions = FilePermissions(0o020);
    /// Execute by group.
    pub const GROUP_EXECUTE: FilePermissions = FilePermissions(0o010);

    /// Read by others.
    pub const OTHERS_READ: FilePermissions = FilePermissions(0o004);
    /// Write by others.
    pub const OTHERS_WRITE: FilePermissions = FilePermissions(0o002);
    /// Execute by others.
    pub const OTHERS_EXECUTE: FilePermissions = FilePermissions(0o001);

    /// Read, write and execute by owner (0700).
    pub const USER_PERMISSIONS: FilePermissions = FilePermissions(0o700);
    /// Read, write and execute by group (0070).
    pub const GROUP_PERMISSIONS: FilePermissions = FilePermissions(0o070);
    /// Read, write and execute by others (0007).
    pub const OTHERS_PERMISSIONS: FilePermissions = FilePermissions(0o007);
    /// Every permission bit (0777).
    pub const ALL_PERMISSIONS: FilePermissions = FilePermissions(0o777);

    /// Default mode of a new file: read and write for everyone (0666), before the umask.
    pub const DEFAULT_FILE_PERMISSIONS: FilePermissions = FilePermissions(0o666);
    /// Default mode of a new directory: every bit (0777), before the umask.
    pub const DEFAULT_DIR_PERMISSIONS: FilePermissions = Self::ALL_PERMISSIONS;
}

impl std::ops::BitOr for FilePermissions {
    type Output = FilePermissions;
    #[inline]
    fn bitor(self, b: FilePermissions) -> FilePermissions {
        FilePermissions(self.0 | b.0)
    }
}

impl std::ops::BitAnd for FilePermissions {
    type Output = FilePermissions;
    #[inline]
    fn bitand(self, b: FilePermissions) -> FilePermissions {
        FilePermissions(self.0 & b.0)
    }
}

impl FilePermissions {
    /// The underlying integer bits.
    #[inline]
    pub const fn bits(self) -> i32 {
        self.0
    }
}
