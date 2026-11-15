//! Файловое хранилище, которое создаёт файлы только при первой записи.
//!
//! Стандартное хранилище librqbit при запуске раздачи создаёт на диске все файлы
//! торрента, даже те, что пользователь не выбрал, — в папке остаются пустые файлы.
//! Здесь файл появляется, только когда в него действительно что-то пишется
//! (или когда движок выставляет длину выбранного файла).

use std::{
    any::TypeId,
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    os::windows::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, RwLock, Weak,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use librqbit::{
    storage::{
        filesystem::FilesystemStorageFactory, BoxStorageFactory, StorageFactory,
        StorageFactoryExt, TorrentStorage,
    },
    ManagedTorrentShared, TorrentMetadata,
};

/// Папки раздач по info hash. librqbit не открывает папку раздачи наружу,
/// поэтому приложение само запоминает, куда какую раздачу добавило.
pub type Folders = Arc<RwLock<HashMap<String, PathBuf>>>;

#[derive(Default, Clone)]
pub struct LazyFsFactory {
    pub folders: Folders,
}

impl StorageFactory for LazyFsFactory {
    type Storage = Box<dyn TorrentStorage>;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        let root = self
            .folders
            .read()
            .unwrap()
            .get(&shared.info_hash.as_string())
            .cloned();
        Ok(match root {
            Some(root) => Box::new(LazyFs {
                root,
                slots: RwLock::new(Arc::new(Vec::new())),
            }),
            // Раздача добавлена старой версией программы — папка неизвестна,
            // работаем через стандартное хранилище.
            None => Box::new(FilesystemStorageFactory::default().create(shared, metadata)?),
        })
    }

    // Сохранение сессии поддерживает только файловое хранилище и узнаёт его по типу.
    // Раскладка файлов на диске у нас та же, так что представляемся им.
    fn is_type_id(&self, type_id: TypeId) -> bool {
        type_id == TypeId::of::<Self>() || type_id == TypeId::of::<FilesystemStorageFactory>()
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

struct Opened {
    file: File,
    writable: bool,
}

struct Slot {
    path: PathBuf,
    padding: bool,
    file: RwLock<Option<Opened>>,
    /// Время последней записи, мс от запуска программы.
    last_write: AtomicU64,
}

pub struct LazyFs {
    root: PathBuf,
    slots: RwLock<Arc<Vec<Slot>>>,
}

// Пока файл открыт на запись, Windows не даёт другим программам запускать его
// и открывать монопольно («файл занят другим процессом»). Поэтому файл, в который
// какое-то время не писали, переоткрывается только для чтения. Этим занимается
// один фоновый поток на всю программу.

const WRITE_IDLE_MS: u64 = 5_000;
const SWEEP_EVERY: Duration = Duration::from_secs(2);

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn registry() -> &'static Mutex<Vec<Weak<Vec<Slot>>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Weak<Vec<Slot>>>>> = OnceLock::new();
    static SWEEPER: OnceLock<()> = OnceLock::new();
    let reg = REGISTRY.get_or_init(|| Mutex::new(Vec::new()));
    SWEEPER.get_or_init(|| {
        std::thread::spawn(move || loop {
            std::thread::sleep(SWEEP_EVERY);
            let lists: Vec<Arc<Vec<Slot>>> = {
                let mut reg = reg.lock().unwrap();
                reg.retain(|w| w.strong_count() > 0);
                reg.iter().filter_map(Weak::upgrade).collect()
            };
            for slot in lists.iter().flat_map(|l| l.iter()) {
                slot.release_write_if_idle();
            }
        });
    });
    reg
}

impl Slot {
    fn write_idle(&self) -> bool {
        now_ms().saturating_sub(self.last_write.load(Ordering::Relaxed)) >= WRITE_IDLE_MS
    }

    fn release_write_if_idle(&self) {
        let writable = matches!(self.file.read().unwrap().as_ref(), Some(o) if o.writable);
        if !writable || !self.write_idle() {
            return;
        }
        let mut guard = self.file.write().unwrap();
        if !self.write_idle() {
            return;
        }
        // Сначала закрываем дескриптор записи, потом открываем на чтение.
        *guard = None;
        if let Ok(file) = OpenOptions::new().read(true).open(&self.path) {
            *guard = Some(Opened {
                file,
                writable: false,
            });
        }
    }
}

/// Разреженный файл: запись в середину не заставляет Windows заполнять нулями всё до неё.
fn mark_sparse(f: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        System::{Ioctl::FSCTL_SET_SPARSE, IO::DeviceIoControl},
    };
    unsafe {
        let _ = DeviceIoControl(
            HANDLE(f.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            None,
            None,
        );
    }
}

impl LazyFs {
    fn slots(&self) -> Arc<Vec<Slot>> {
        self.slots.read().unwrap().clone()
    }

    /// Выполняет операцию над файлом, открывая его при необходимости.
    /// `write = false` — только существующий файл, открывается на чтение;
    /// `write = true` — файл создаётся при необходимости и открывается на запись.
    fn with_file<T>(
        &self,
        file_id: usize,
        write: bool,
        op: impl FnOnce(&File) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        let slots = self.slots();
        let slot = slots.get(file_id).context("no such file")?;
        if write {
            slot.last_write.store(now_ms(), Ordering::Relaxed);
        }
        if let Some(o) = slot.file.read().unwrap().as_ref() {
            if !write || o.writable {
                return Ok(op(&o.file)?);
            }
        }
        let mut guard = slot.file.write().unwrap();
        let reopen = match guard.as_ref() {
            Some(o) => write && !o.writable,
            None => true,
        };
        if reopen {
            *guard = None;
            let file = if write {
                if let Some(parent) = slot.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&slot.path)
                    .with_context(|| format!("error opening {:?} for writing", slot.path))?;
                mark_sparse(&f);
                f
            } else {
                OpenOptions::new()
                    .read(true)
                    .open(&slot.path)
                    .with_context(|| format!("error opening {:?}", slot.path))?
            };
            *guard = Some(Opened {
                file,
                writable: write,
            });
        }
        Ok(op(&guard.as_ref().unwrap().file)?)
    }

    fn is_padding(&self, file_id: usize) -> bool {
        self.slots()
            .get(file_id)
            .map(|s| s.padding)
            .unwrap_or(false)
    }
}

impl TorrentStorage for LazyFs {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        let slots: Arc<Vec<Slot>> = Arc::new(
            metadata
                .file_infos
                .iter()
                .map(|fi| Slot {
                    path: self.root.join(&fi.relative_filename),
                    padding: fi.attrs.padding,
                    file: RwLock::new(None),
                    last_write: AtomicU64::new(0),
                })
                .collect(),
        );
        registry().lock().unwrap().push(Arc::downgrade(&slots));
        *self.slots.write().unwrap() = slots;
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        if self.is_padding(file_id) {
            buf.fill(0);
            return Ok(());
        }
        self.with_file(file_id, false, |f| {
            let mut done = 0;
            while done < buf.len() {
                match f.seek_read(&mut buf[done..], offset + done as u64)? {
                    0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                    n => done += n,
                }
            }
            Ok(())
        })
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        if self.is_padding(file_id) {
            return Ok(());
        }
        self.with_file(file_id, true, |f| {
            let mut done = 0;
            while done < buf.len() {
                match f.seek_write(&buf[done..], offset + done as u64)? {
                    0 => return Err(io::ErrorKind::WriteZero.into()),
                    n => done += n,
                }
            }
            Ok(())
        })
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        if let Some(slot) = self.slots().get(file_id) {
            *slot.file.write().unwrap() = None;
        }
        match std::fs::remove_file(self.root.join(filename)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.root.join(path);
        if path.is_dir() && std::fs::read_dir(&path)?.next().is_none() {
            std::fs::remove_dir(&path).with_context(|| format!("error removing {path:?}"))?;
        }
        Ok(())
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        if self.is_padding(file_id) {
            return Ok(());
        }
        // Длина уже верная (обычный случай при повторном запуске) — файл не трогаем
        // и не открываем на запись.
        if let Some(slot) = self.slots().get(file_id) {
            if std::fs::metadata(&slot.path).map(|m| m.len()).ok() == Some(length) {
                return Ok(());
            }
        }
        self.with_file(file_id, true, |f| f.set_len(length))
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let slots = std::mem::take(&mut *self.slots.write().unwrap());
        Ok(Box::new(LazyFs {
            root: self.root.clone(),
            slots: RwLock::new(slots),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use librqbit::{AddTorrent, AddTorrentOptions, DhtSessionConfig, Session, SessionOptions};

    use super::*;

    /// Качает один файл из раздачи Sintel и проверяет, что:
    /// - невыбранные файлы, в которые ничего не писалось, на диске не появились;
    /// - скачанный файл после загрузки не остаётся открытым на запись.
    /// Запуск: cargo test -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "нужна сеть"]
    async fn creates_only_touched_files() {
        let hash = "08ada5a7a6183aae1e09d831df6748d566095a10";
        let dir = std::env::temp_dir().join(format!("phoenix-test-{}", std::process::id()));
        let factory = LazyFsFactory::default();
        factory
            .folders
            .write()
            .unwrap()
            .insert(hash.into(), dir.clone());

        let session = Session::new_with_opts(
            dir.clone(),
            SessionOptions {
                default_storage_factory: Some(factory.boxed()),
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let magnet = format!(
            "magnet:?xt=urn:btih:{hash}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=udp%3A%2F%2Fexplodie.org%3A6969"
        );
        let handle = session
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    only_files: Some(vec![0]),
                    output_folder: Some(dir.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        tokio::time::timeout(Duration::from_secs(240), handle.wait_until_completed())
            .await
            .expect("не скачалось за 4 минуты")
            .unwrap();

        let files: Vec<(PathBuf, u64)> = handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .map(|f| (f.relative_filename.clone(), f.len))
                    .collect()
            })
            .unwrap();
        for (i, (name, len)) in files.iter().enumerate() {
            let on_disk = std::fs::metadata(dir.join(name)).map(|m| m.len()).ok();
            println!("{i:2} {name:?} torrent={len} disk={on_disk:?}");
        }

        let (first, first_len) = &files[0];
        assert_eq!(std::fs::metadata(dir.join(first)).unwrap().len(), *first_len);
        let created = files.iter().filter(|(n, _)| dir.join(n).exists()).count();
        assert!(created < files.len(), "созданы все файлы раздачи");
        let (last, _) = files.last().unwrap();
        assert!(!dir.join(last).exists(), "создан файл, в который ничего не писалось");

        // Так файл открывает загрузчик Windows при запуске exe: «другим — только чтение».
        // Пока раздача держит файл открытым на запись, это падает с ошибкой
        // «процесс не может получить доступ к файлу».
        const FILE_SHARE_READ: u32 = 1;
        let open_like_loader = || {
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(dir.join(first))
        };
        println!("сразу после загрузки: {:?}", open_like_loader().map(|_| ()));
        tokio::time::sleep(Duration::from_millis(WRITE_IDLE_MS) + SWEEP_EVERY * 2).await;
        open_like_loader().expect("файл остался открытым на запись");

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
