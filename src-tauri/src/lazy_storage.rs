//! Файловое хранилище, которое создаёт файлы только при первой записи.
//!
//! Стандартное хранилище librqbit при запуске раздачи создаёт на диске все файлы
//! торрента, даже те, что пользователь не выбрал, — в папке остаются пустые файлы.
//! Здесь файл появляется, только когда в него действительно что-то пишется
//! (или когда движок выставляет длину выбранного файла).

use std::{
    any::TypeId,
    collections::HashMap,
    sync::Arc,
    fs::{File, OpenOptions},
    io,
    os::windows::fs::FileExt,
    path::{Path, PathBuf},
    sync::RwLock,
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
                slots: RwLock::new(Vec::new()),
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

struct Slot {
    path: PathBuf,
    padding: bool,
    file: RwLock<Option<File>>,
}

pub struct LazyFs {
    root: PathBuf,
    slots: RwLock<Vec<Slot>>,
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
    /// Выполняет операцию над файлом, открывая его при необходимости.
    /// `create = false` — только существующий файл (чтение), иначе файл создаётся.
    fn with_file<T>(
        &self,
        file_id: usize,
        create: bool,
        op: impl FnOnce(&File) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        let slots = self.slots.read().unwrap();
        let slot = slots.get(file_id).context("no such file")?;
        if let Some(f) = slot.file.read().unwrap().as_ref() {
            return Ok(op(f)?);
        }
        let mut guard = slot.file.write().unwrap();
        if guard.is_none() {
            if create {
                if let Some(parent) = slot.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(&slot.path)
                .with_context(|| format!("error opening {:?}", slot.path))?;
            mark_sparse(&f);
            *guard = Some(f);
        }
        Ok(op(guard.as_ref().unwrap())?)
    }

    fn is_padding(&self, file_id: usize) -> bool {
        self.slots
            .read()
            .unwrap()
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
        let slots = metadata
            .file_infos
            .iter()
            .map(|fi| Slot {
                path: self.root.join(&fi.relative_filename),
                padding: fi.attrs.padding,
                file: RwLock::new(None),
            })
            .collect();
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
        if let Some(slot) = self.slots.read().unwrap().get(file_id) {
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
    use std::time::Duration;

    use librqbit::{AddTorrent, AddTorrentOptions, DhtSessionConfig, Session, SessionOptions};

    use super::*;

    /// Качает один файл из раздачи Sintel и проверяет, что невыбранные файлы,
    /// в которые ничего не писалось, на диске не появились.
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

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}