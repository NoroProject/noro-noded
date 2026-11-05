// Файл превышает 150 строк: системные вызовы project quota и обход каталога по дескрипторам — одно место с unsafe.
//! Project quota на уровне ядра: `ioctl(FS_IOC_FSSETXATTR)` и `quotactl_fd`.
//!
//! Без внешних программ: `xfs_quota` и `setquota` — разные пакеты под разные
//! файловые системы, а эти два вызова одинаковы для xfs и ext4.
//! `quotactl_fd` (Linux 5.14+) берёт дескриптор каталога вместо пути к
//! блочному устройству, которого демон в контейнере и на LVM не знает.

#[cfg(target_os = "linux")]
pub use linux::open;

#[cfg(not(target_os = "linux"))]
pub use other::open;

#[cfg(not(target_os = "linux"))]
mod other {
    use std::path::Path;

    /// Вне Linux project quota нет; тип нужен только чтобы код собирался.
    pub struct Kernel;

    pub fn open(_: &Path) -> std::io::Result<Kernel> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "project quota is Linux-only",
        ))
    }

    impl super::super::project::ProjectFs for Kernel {
        fn assign(&self, _: &Path, _: u32) -> std::io::Result<()> {
            unreachable!()
        }
        fn set_limit(&self, _: u32, _: u64) -> std::io::Result<()> {
            unreachable!()
        }
        fn usage(&self, _: u32) -> std::io::Result<u64> {
            unreachable!()
        }
        fn in_use(&self, _: u32) -> std::io::Result<bool> {
            unreachable!()
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{Error, Result};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const PRJQUOTA: u32 = 2;
    /// `struct fsxattr` из `linux/fs.h`.
    #[repr(C)]
    #[derive(Default)]
    struct FsXattr {
        xflags: u32,
        extsize: u32,
        nextents: u32,
        projid: u32,
        cowextsize: u32,
        pad: [u8; 8],
    }
    // _IOR('X', 31, struct fsxattr) и _IOW('X', 32, struct fsxattr): кодировка
    // общая для x86_64 и arm64.
    const FS_IOC_FSGETXATTR: u32 = 0x801C_581F;
    const FS_IOC_FSSETXATTR: u32 = 0x401C_5820;
    const FS_XFLAG_PROJINHERIT: u32 = 0x200;

    /// Том каталога данных: через его дескриптор идут все `quotactl_fd`.
    pub struct Kernel {
        volume: File,
    }

    /// Открыть том и убедиться, что квоты проектов на нём включены: без этого
    /// `ESRCH`, на старом ядре — `ENOSYS`.
    pub fn open(data_dir: &Path) -> Result<Kernel> {
        let kernel = Kernel {
            volume: File::open(data_dir)?,
        };
        kernel.get(0)?;
        Ok(kernel)
    }

    impl Kernel {
        fn quotactl(&self, cmd: libc::c_int, id: u32, dq: &mut libc::dqblk) -> Result<()> {
            let cmd = ((cmd as u32) << 8) | PRJQUOTA;
            // SAFETY: дескриптор тома жив, `dq` — настоящая структура нужного
            // размера и живёт до конца вызова.
            let r = unsafe {
                libc::syscall(
                    libc::SYS_quotactl_fd,
                    self.volume.as_raw_fd() as libc::c_uint,
                    cmd,
                    id,
                    dq as *mut libc::dqblk,
                )
            };
            if r < 0 {
                Err(Error::last_os_error())
            } else {
                Ok(())
            }
        }

        /// Запись проекта. Её отсутствие (xfs отвечает `ENOENT`) — пустой проект.
        fn get(&self, id: u32) -> Result<libc::dqblk> {
            // SAFETY: dqblk — набор целых, нули для него допустимы.
            let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
            match self.quotactl(libc::Q_GETQUOTA, id, &mut dq) {
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(dq),
                other => other.map(|_| dq),
            }
        }
    }

    impl super::super::project::ProjectFs for Kernel {
        fn assign(&self, root: &Path, id: u32) -> Result<()> {
            let dir = super::super::nofollow::Dir::open(root)?.fd()?;
            walk(&dir, id)
        }

        fn set_limit(&self, id: u32, bytes: u64) -> Result<()> {
            // SAFETY: см. `get`.
            let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
            // Блоки квоты — по килобайту (QIF_DQBLKSIZE), на всех файловых системах.
            dq.dqb_bhardlimit = bytes.div_ceil(1024);
            dq.dqb_valid = libc::QIF_BLIMITS;
            self.quotactl(libc::Q_SETQUOTA, id, &mut dq)
        }

        fn usage(&self, id: u32) -> Result<u64> {
            Ok(self.get(id)?.dqb_curspace)
        }

        fn in_use(&self, id: u32) -> Result<bool> {
            let dq = self.get(id)?;
            Ok(dq.dqb_curspace > 0 || dq.dqb_curinodes > 0 || dq.dqb_bhardlimit > 0)
        }
    }

    /// Пометить каталог (с наследованием) и всё под ним, не идя по ссылкам.
    ///
    /// Сначала сам каталог, потом его содержимое: созданное сервером, пока
    /// обход идёт, наследует номер от уже помеченного родителя.
    fn walk(dir: &OwnedFd, id: u32) -> Result<()> {
        set_project(dir, id, true)?;
        let listing = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?;
        for entry in listing {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !(kind.is_dir() || kind.is_file()) {
                continue;
            }
            // Запись могли удалить или подменить ссылкой между чтением
            // каталога и открытием — тогда её просто нет для обхода.
            let Some(fd) = open_at(dir, &entry.file_name(), kind.is_dir())? else {
                continue;
            };
            if kind.is_dir() {
                walk(&fd, id)?;
            } else {
                set_project(&fd, id, false)?;
            }
        }
        Ok(())
    }

    fn open_at(parent: &OwnedFd, name: &OsStr, dir: bool) -> Result<Option<OwnedFd>> {
        let c = std::ffi::CString::new(name.as_bytes()).map_err(Error::other)?;
        let mut flags =
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
        if dir {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: дескриптор родителя жив, имя — строка с нулём.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), c.as_ptr(), flags) };
        if fd < 0 {
            let e = Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR) => Ok(None),
                _ => Err(e),
            };
        }
        // SAFETY: `openat` только что вернул этот дескриптор.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // Файл, подменённый очередью или сокетом, ioctl не примет — и метить
        // в нём нечего.
        let meta = File::from(fd.try_clone()?).metadata()?;
        let expected = if dir { meta.is_dir() } else { meta.is_file() };
        Ok(expected.then_some(fd))
    }

    fn set_project(fd: &OwnedFd, id: u32, inherit: bool) -> Result<()> {
        let mut attr = FsXattr::default();
        // SAFETY: дескриптор жив, `attr` — структура ровно того размера, что
        // зашит в номер запроса.
        if unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_FSGETXATTR as _, &mut attr) } < 0 {
            return Err(Error::last_os_error());
        }
        let flag = if inherit { FS_XFLAG_PROJINHERIT } else { 0 };
        if attr.projid == id && attr.xflags & flag == flag {
            return Ok(());
        }
        attr.projid = id;
        attr.xflags |= flag;
        // SAFETY: см. выше.
        if unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_FSSETXATTR as _, &attr) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        /// Номер запроса зашивает размер структуры: разъедется — ядро ответит
        /// `ENOTTY` на каждый вызов.
        #[test]
        fn fsxattr_has_the_kernel_layout() {
            assert_eq!(std::mem::size_of::<super::FsXattr>(), 28);
            assert_eq!((super::FS_IOC_FSGETXATTR >> 16) & 0x3fff, 28);
            assert_eq!((super::FS_IOC_FSSETXATTR >> 16) & 0x3fff, 28);
        }

        /// Настоящее ядро: нужен том с prjquota и root. Проверяет то, чего
        /// фейк не может, — что ядро принимает наши вызовы и режет запись.
        ///
        /// ```sh
        /// truncate -s 64M /tmp/x.img && mkfs.ext4 -q -O quota,project /tmp/x.img
        /// mkdir /mnt/q && mount -o loop,prjquota /tmp/x.img /mnt/q
        /// NORO_PRJQUOTA_DIR=/mnt/q cargo test -- --ignored the_kernel
        /// ```
        #[test]
        #[ignore]
        fn the_kernel_refuses_writes_past_the_limit() {
            use crate::fs::project::ProjectFs;
            use std::io::Write;
            let volume = std::path::PathBuf::from(std::env::var("NORO_PRJQUOTA_DIR").unwrap());
            let kernel = super::open(&volume).expect("project quota включена");
            let root = volume.join(format!("server-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("world")).unwrap();
            std::fs::write(root.join("world/old.dat"), vec![1u8; 1024 * 1024]).unwrap();

            let id = 4242;
            kernel.assign(&root, id).unwrap();
            assert!(
                kernel.usage(id).unwrap() >= 1024 * 1024,
                "старый файл посчитан"
            );
            kernel.set_limit(id, 4 * 1024 * 1024).unwrap();

            let mut file = std::fs::File::create(root.join("world/new.dat")).unwrap();
            let refused = (0..16).find_map(|_| {
                file.write_all(&vec![2u8; 1024 * 1024])
                    .and_then(|_| file.sync_all())
                    .err()
            });
            let error = refused.expect("запись сверх лимита отказана");
            assert_eq!(error.raw_os_error(), Some(libc::EDQUOT));
            assert!(kernel.in_use(id).unwrap());

            std::fs::remove_dir_all(&root).unwrap();
            kernel.set_limit(id, 0).unwrap();
        }
    }
}
