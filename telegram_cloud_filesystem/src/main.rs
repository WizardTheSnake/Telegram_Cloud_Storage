//! Example to mount media from a telegram chat to virtual filesystem.
//!
//! The `TG_ID` and `TG_HASH` environment variables must be set 
//! To run:
//! ```sh
//! cargo run --example downloader -- ~/path/where/to/mount
//! ```

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use std::{env, io};

use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyEntry, Request};
use grammers_client::{Client, Config, SignInError};
use libc::ENOENT;
use mime::Mime;
use mime_guess::mime;
use simple_logger::SimpleLogger;
use tokio::runtime::Runtime;

use grammers_client::session::Session;
use grammers_client::types::Media::{self, Contact, Document, Photo, Sticker};
use std::sync::RwLock;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const SESSION_FILE: &str = "downloader.session";

const TTL: Duration = Duration::from_secs(1); // 1 second

const DIR_ATTR: FileAttr = FileAttr {
    ino: 1,
    size: 0,
    blocks: 0,
    atime: UNIX_EPOCH, // 1970-01-01 00:00:00
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::Directory,
    perm: 0o755,
    nlink: 2,
    uid: 501,
    gid: 20,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

pub struct CachedFile {
    pub ino: u64,
    pub name: String,
    pub content: Arc<Vec<u8>>,
    pub attr: FileAttr,
}

struct TelegramClient{
    my_client: Client,
    rt: Runtime,
    //cache: Arc<RwLock<Vec<CachedFile>>>,
    cache: Arc<RwLock<HashMap<String, Vec<CachedFile>>>>,
}

impl TelegramClient {
    fn init() -> Self {
    // 1. Utwórz runtime
        let rt = Runtime::new().unwrap();

        // 2. Uruchom wszystko wewnątrz runtime
        let client = rt.block_on(async {
            SimpleLogger::new()
                .with_level(log::LevelFilter::Info)
                .init()
                .unwrap();

            let api_id = env!("TG_ID").parse().expect("TG_ID invalid");
            let api_hash = env!("TG_HASH").to_string();

            println!("Connecting to Telegram...");
            let client = Client::connect(Config {
                session: Session::load_file_or_create(SESSION_FILE).unwrap(),
                api_id,
                api_hash: api_hash.clone(),
                params: Default::default(),
            })
            .await
            .unwrap();
            println!("Connected!");

            if !client.is_authorized().await.unwrap() {
                println!("Signing in...");
                let phone = prompt("Enter your phone number (international format): ").unwrap();
                let token = client.request_login_code(&phone).await.unwrap();
                let code = prompt("Enter the code you received: ").unwrap();
                let signed_in = client.sign_in(&token, &code).await;

                match signed_in {
                    Err(SignInError::PasswordRequired(password_token)) => {
                        let hint = password_token.hint().unwrap();
                        let prompt_message = format!("Enter the password (hint {}): ", &hint);
                        let password = prompt(prompt_message.as_str()).unwrap();

                        client
                            .check_password(password_token, password.trim())
                            .await
                            .unwrap();
                    }
                    Ok(_) => (),
                    Err(e) => panic!("{}", e),
                }

                match client.session().save_to_file(SESSION_FILE) {
                    Ok(_) => {}
                    Err(e) => {
                        println!(
                            "NOTE: failed to save the session, will sign out when done: {e}"
                        );
                    }
                }
                println!("Signed in!");
            }

            client
        });

        // 3. Zwróć gotową strukturę
        Self {
            my_client: client,
            rt,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn spawn_cache_updater(&self) {
        let my_client = self.my_client.clone(); // Clone
        let cache = Arc::clone(&self.cache);

        self.rt.spawn(async move {
            loop {
                let mut dialogs = my_client.iter_dialogs();

                while let Some(dialog) = dialogs.next().await.unwrap() {
                        if let Some(name) = dialog.chat().name() {
                            let mut files = vec![];
                            let folder_ino = TelegramFS::folder_ino(&name);
                            let mut ino_counter = folder_ino+1;

                            let mut messages = my_client.iter_messages(&dialog.chat);

                            while let Some(msg) = messages.next().await.unwrap() {
                                if let Some(media) = msg.media() {
                                    let file_name = format!("msg-{}{}", msg.id(), get_file_extension(&media));
                                    let mut buf: Vec<u8> = vec![];
                                    let mut stream = my_client.iter_download(&media);

                                    while let Some(chunk) = stream.next().await.unwrap() {
                                        buf.extend_from_slice(&chunk);
                                    }

                                    let content = Arc::new(buf);
                                    let attr = FileAttr {
                                        ino: ino_counter,
                                        size: content.len() as u64,
                                        blocks: 1,
                                        atime: UNIX_EPOCH,
                                        mtime: UNIX_EPOCH,
                                        ctime: UNIX_EPOCH,
                                        crtime: UNIX_EPOCH,
                                        kind: FileType::RegularFile,
                                        perm: 0o644,
                                        nlink: 1,
                                        uid: 501,
                                        gid: 20,
                                        rdev: 0,
                                        flags: 0,
                                        blksize: 512,
                                    };

                                    files.push(CachedFile {
                                        ino: ino_counter,
                                        name: file_name,
                                        content,
                                        attr,
                                    });

                                    ino_counter += 1;
                                }
                            }

                            if !files.is_empty() {
                                // Zapisz pliki dla danego folderu
                                cache.write().unwrap().insert(name.to_string(), files);
                            }
                        }
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }
}
struct TelegramFS
{
    client: TelegramClient,
}

impl TelegramFS {
    pub fn init() -> Self {
        let client = TelegramClient::init();
        client.spawn_cache_updater(); // startujemy aktualizację

        Self { client }
    }
    fn folder_ino(name: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish()
    }
}

impl Filesystem for TelegramFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_str().unwrap_or("");
        let cache = self.client.cache.read().unwrap();

        if parent == 1 {
            // Szukamy folderu (czyli czatu)
            if cache.contains_key(name) {
                let attr = FileAttr {
                    ino: Self::folder_ino(name),
                    size: 0,
                    blocks: 0,
                    atime: UNIX_EPOCH,
                    mtime: UNIX_EPOCH,
                    ctime: UNIX_EPOCH,
                    crtime: UNIX_EPOCH,
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid: 501,
                    gid: 20,
                    rdev: 0,
                    flags: 0,
                    blksize: 512,
                };
                reply.entry(&TTL, &attr, 0);
                return;
            }
        } else {
            // Szukamy pliku w folderze
            if let Some((_, files)) = cache.iter().find(|(folder_name, _)| Self::folder_ino(folder_name) == parent) {
                if let Some(file) = files.iter().find(|f| f.name == name) {
                    reply.entry(&TTL, &file.attr, 0);
                    return;
                }
            }
        }

        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&TTL, &DIR_ATTR);
            return;
        }

        let cache = self.client.cache.read().unwrap();

        // Czy to folder (na podstawie folder_ino)?
        for folder_name in cache.keys() {
            if TelegramFS::folder_ino(folder_name) == ino {
                let attr = FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: UNIX_EPOCH,
                    mtime: UNIX_EPOCH,
                    ctime: UNIX_EPOCH,
                    crtime: UNIX_EPOCH,
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid: 501,
                    gid: 20,
                    rdev: 0,
                    flags: 0,
                    blksize: 512,
                };
                reply.attr(&TTL, &attr);
                return;
            }
        }

        // Szukaj pliku
        for files in cache.values() {
            if let Some(file) = files.iter().find(|f| f.ino == ino) {
                reply.attr(&TTL, &file.attr);
                return;
            }
        }

        reply.error(ENOENT);
    }

    fn readdir(
    &mut self,
    _req: &fuser::Request<'_>,
    ino: u64,
    _fh: u64,
    offset: i64,
    mut reply: fuser::ReplyDirectory,) {
        let cache = self.client.cache.read().unwrap();

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (1, FileType::Directory, "..".to_string()),
        ];

        if ino == 1 {
            // W katalogu głównym: wypisz foldery (czaty z plikami)
            for folder_name in cache.keys() {
                entries.push((Self::folder_ino(folder_name), FileType::Directory, folder_name.clone()));
            }
        } else {
            // Szukaj folderu o pasującym ino
            if let Some((_, files)) = cache.iter().find(|(name, _)| Self::folder_ino(name) == ino) {
                for file in files {
                    entries.push((file.ino, FileType::RegularFile, file.name.clone()));
                }
            } else {
                reply.error(ENOENT);
                return;
            }
        }

        for (i, (ino, kind, name)) in entries.into_iter().skip(offset as usize).enumerate() {
            if reply.add(ino, offset + i as i64 + 1, kind, name) {
                break;
            }
        }

        reply.ok();
    }

    fn read( &mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData,) {
        let cache = self.client.cache.read().unwrap();

        for files in cache.values() {
            if let Some(file) = files.iter().find(|f| f.ino == ino) {
                let data = &file.content;
                let start = offset as usize;
                let end = std::cmp::min(start + size as usize, data.len());
                reply.data(&data[start..end]);
                return;
            }
        }

        reply.error(ENOENT);
    }    
}

fn main() {
    // Używamy np. `clap` albo `std::env` do pobrania ścieżki montowania
    let mountpoint = env::args().nth(1).expect("Usage: ./program <mountpoint>");

    // Inicjalizacja naszego FS
    let fs = TelegramFS::init();

    // Montujemy
    fuser::mount2(
        fs,
        mountpoint,
        &[
            MountOption::RO,
            MountOption::FSName("telegramfs".into()),
            MountOption::AutoUnmount,
            MountOption::AllowOther, 
        ],
    ).unwrap();
}

fn get_file_extension(media: &Media) -> String {
    match media {
        Photo(_) => ".jpg".to_string(),
        Sticker(sticker) => get_mime_extension(sticker.document.mime_type()),
        Document(document) => get_mime_extension(document.mime_type()),
        Contact(_) => ".vcf".to_string(),
        _ => String::new(),
    }
}

fn get_mime_extension(mime_type: Option<&str>) -> String {
    mime_type
        .and_then(|m| m.parse::<Mime>().ok())
        .map(|mime| format!(".{}", mime.subtype()))
        .unwrap_or(".bin".to_string())
}

fn prompt(message: &str) -> Result<String> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()?;

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line)
}