//! Example to mount media from a telegram chat to virtual filesystem.
//!
//! The `TG_ID` and `TG_HASH` environment variables must be set 
//! Instruction to get TG_ID and TG_HASH: https://core.telegram.org/api/obtaining_api_id#obtaining-api-id
//! 
//! To run:
//! open terminal in Telegram_Cloud_Storage directory and type:
//! cargo run --bin telegram_cloud_filesystem ~/path/where/to/mount

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use std::{env, io};

use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyEntry, Request};
use grammers_client::types::Downloadable;
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

// This allows resuming sessions without re-authenticating every time.
const SESSION_FILE: &str = "downloader.session";

// Time-To-Live for cached file attributes in the virtual filesystem.
const TTL: Duration = Duration::from_secs(1); // 1 second

// This describes a directory inode with standard permissions and timestamps
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

/* Structure representing a cached file in the virtual filesystem.
Each CachedFile stores:
- a unique inode number (`ino`)
- the filename as a String (`name`)
- the actual file content stored in an Arc<Vec<u8>> for thread-safe sharing
- file attributes (`attr`) like size, permissions, timestamps, etc.*/
pub struct CachedFile {
    pub ino: u64,
    pub name: String,
    pub content: Arc<Vec<u8>>,
    pub attr: FileAttr,
}

/* Main Telegram client structure that manages:
- The grammers Client instance to communicate with Telegram API.
- A Tokio Runtime for async execution.
- A cache that maps folder names (chat names) to a vector of CachedFiles representing messages/media in that folder.
This cache is protected by a read-write lock and wrapped in Arc for safe concurrent access.*/
struct TelegramClient{
    my_client: Client,
    rt: Runtime,
    //cache: Arc<RwLock<Vec<CachedFile>>>,
    cache: Arc<RwLock<HashMap<String, Vec<CachedFile>>>>,
}

impl TelegramClient {
    fn init() -> Self {
    // 1. Create a new Tokio runtime for asynchronous tasks
        let rt = Runtime::new().unwrap();

        // 2. Run all async code inside the runtime context
        let client = rt.block_on(async {
            SimpleLogger::new()
                .with_level(log::LevelFilter::Info)
                .init()
                .unwrap();
            
            // Read Telegram API credentials from environment variables
            let api_id = env!("TG_ID").parse().expect("TG_ID invalid");
            let api_hash = env!("TG_HASH").to_string();

            println!("Connecting to Telegram...");
            // Connect to Telegram client with session or create new session file
            let client = Client::connect(Config {
                session: Session::load_file_or_create(SESSION_FILE).unwrap(),
                api_id,
                api_hash: api_hash.clone(),
                params: Default::default(),
            })
            .await
            .unwrap();
            println!("Connected!");
            
            // Check if client is authorized (logged in)
            if !client.is_authorized().await.unwrap() {
                println!("Signing in...");
                let phone = prompt("Enter your phone number (international format): ").unwrap();
                let token = client.request_login_code(&phone).await.unwrap();
                let code = prompt("Enter the code you received: ").unwrap();
                let signed_in = client.sign_in(&token, &code).await;
                
                // Handle two-factor password if required
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
                
                // Save the authorized session to the file for future reuse
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
            
            // Return the connected and authorized client
            client
        });

        // 3. Return the TelegramClient structure with client, runtime, and empty cache
        Self {
            my_client: client,
            rt,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn spawn_cache_updater(&self) {
        // Clone the Telegram client and the shared cache so they can be moved into the async task
        let my_client = self.my_client.clone();
        let cache = Arc::clone(&self.cache);

        // Spawn an asynchronous task on the runtime to update the cache continuously
        self.rt.spawn(async move {
            loop {
                // Iterate over all Telegram dialogs (chats, channels, groups)
                let mut dialogs = my_client.iter_dialogs();
                
                // Process each dialog one by one
                while let Some(dialog) = dialogs.next().await.unwrap() {
                    let name = dialog.chat().name();
                    if !name.is_empty() {
                        let mut files = vec![];

                        // Calculate a base inode number for this folder (chat) to uniquely identify files
                        let folder_ino = TelegramFS::folder_ino(&name);
                        let mut ino_counter = folder_ino + 1;
                        
                        // Iterate over all messages in the dialog
                        let mut messages = my_client.iter_messages(&dialog.chat);

                        while let Some(msg) = messages.next().await.unwrap() {
                            // Check if message contains media (file/photo/video/etc.)
                            if let Some(media) = msg.media() {
                                // Construct a filename using message ID and media file extension
                                let file_name = format!("msg-{}{}", msg.id(), get_file_extension(&media));
                                
                                // Create a Downloadable object for the media
                                let downloadable = Downloadable::Media(media.clone()); // <-- Poprawka
                                // Buffer to accumulate downloaded bytes
                                let mut buf: Vec<u8> = vec![];
                                // Create a stream to download the media in chunks
                                let mut stream = my_client.iter_download(&downloadable); // <-- Poprawka
                                // Download chunks and append them to the buffer
                                while let Some(chunk) = stream.next().await.unwrap() {
                                    buf.extend_from_slice(&chunk);
                                }
                                // Wrap the buffer in an Arc for shared ownership
                                let content = Arc::new(buf);
                                // Create file attributes for this cached file (metadata)
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
                                 // Push the cached file representation into the files vector
                                files.push(CachedFile {
                                    ino: ino_counter,
                                    name: file_name,
                                    content,
                                    attr,
                                });
                                // Increment inode counter for next file
                                ino_counter += 1;
                            }
                        }
                        // If there are files found in this dialog, update the cache with them
                        if !files.is_empty() {
                            cache.write().unwrap().insert(name.to_string(), files);
                        }
                    }
                }
                // Sleep for 10 seconds before refreshing the cache again
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
    // Initialize TelegramFS by creating a TelegramClient and starting the cache updater task
    pub fn init() -> Self {
        let client = TelegramClient::init();
        client.spawn_cache_updater(); // start cache update loop in background

        Self { client }
    }
    // Generate a unique inode number for a folder (chat) based on its name
    fn folder_ino(name: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish()
    }
}

impl Filesystem for TelegramFS {

    /*The `lookup` method is called by the filesystem when
    the OS wants to resolve a filename within a given directory (inode).
    It checks if the requested name exists as a folder (Telegram chat) when
    the parent inode is 1 (root folder), or as a file inside a folder otherwise.
    If found, it replies with the file or folder metadata (attributes),
    otherwise returns a "not found" error.*/
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_str().unwrap_or("");
        // Acquire a read lock on the cached files
        let cache = self.client.cache.read().unwrap();

        if parent == 1 {
            // Parent inode 1 means we are looking for a folder (Telegram chat)
            if cache.contains_key(name) {
                let attr = FileAttr {
                    ino: Self::folder_ino(name), // generate inode for the folder
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
                // Reply with the directory entry and TTL (cache timeout)
                reply.entry(&TTL, &attr, 0);
                return;
            }
        } else {
            // Otherwise, we are looking for a file inside a folder
            // Find the folder name by matching the inode number
            if let Some((_, files)) = cache.iter().find(|(folder_name, _)| Self::folder_ino(folder_name) == parent) {
                // Find the file by its name inside the folder's files
                if let Some(file) = files.iter().find(|f| f.name == name) {
                    // Reply with the file entry attributes and TTL
                    reply.entry(&TTL, &file.attr, 0);
                    return;
                }
            }
        }
        // If no matching folder or file is found, reply with ENOENT (not found)
        reply.error(ENOENT);
    }


    /*  The `getattr` method returns the metadata (attributes) of a file or directory
    identified by its inode number `ino`.
    If the inode is 1, it means the root directory, so reply with predefined root directory attributes.
    Otherwise, check if the inode matches a Telegram chat folder or a file in the cache.
    If found, respond with the appropriate attributes; if not, return an error.*/
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == 1 {
            // Root directory inode
            reply.attr(&TTL, &DIR_ATTR);
            return;
        }
        // Acquire read lock on the cache to access cached Telegram chats and files
        let cache = self.client.cache.read().unwrap();

        // Check if inode corresponds to a folder (Telegram chat)
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

        
        // Otherwise, look for a file with matching inode inside cached folders
        for files in cache.values() {
            if let Some(file) = files.iter().find(|f| f.ino == ino) {
                reply.attr(&TTL, &file.attr);
                return;
            }
        }
        
        // If inode not found, return "not found" error
        reply.error(ENOENT);
    }


    /* The `readdir` method lists directory contents based on the given `ino` (inode number).
    If `ino == 1`, this is the root directory, and we return a list of chat folders (Telegram dialogs).
    Otherwise, we treat it as a chat folder and return the list of media files associated with that chat.
    The `offset` is used by FUSE for pagination; we skip entries up to the given offset.
    We must call `reply.add()` for each entry, and finally `reply.ok()` to finish.*/
    fn readdir( &mut self, _req: &fuser::Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: fuser::ReplyDirectory,) {
        let cache = self.client.cache.read().unwrap();

        // Initial entries: "." (self) and ".." (parent)
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (1, FileType::Directory, "..".to_string()),
        ];

        if ino == 1 {
            // We're in the root directory. List all folders (Telegram chats).
            for folder_name in cache.keys() {
                entries.push((Self::folder_ino(folder_name), FileType::Directory, folder_name.clone()));
            }
        } else {
            // We're in a chat folder. Find the matching chat and list its media files.
            if let Some((_, files)) = cache.iter().find(|(name, _)| Self::folder_ino(name) == ino) {
                for file in files {
                    entries.push((file.ino, FileType::RegularFile, file.name.clone()));
                }
            } else {
                // No matching chat folder found → return error
                reply.error(ENOENT);
                return;
            }
        }

        // Emit directory entries starting from the given offset
        for (i, (ino, kind, name)) in entries.into_iter().skip(offset as usize).enumerate() {
            if reply.add(ino, offset + i as i64 + 1, kind, name) {
                break;
            }
        }

        reply.ok(); // Signal successful directory listing
    }

    /* This method handles reading data from a file identified by `ino` (inode number).
    It returns up to `size` bytes starting from `offset`.
    The file content is retrieved from the in-memory cache.*/
    fn read( &mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData,) {
        let cache = self.client.cache.read().unwrap();
        
        // Search all cached chat folders
        for files in cache.values() {
            // Look for the file with the matching inode number
            if let Some(file) = files.iter().find(|f| f.ino == ino) {
                let data = &file.content;
                let start = offset as usize;
                // Ensure we don't read past the end of the file
                let end = std::cmp::min(start + size as usize, data.len());
                // Return the slice of data from offset to end
                reply.data(&data[start..end]);
                return;
            }
        }
        // If no matching file was found, return an error
        reply.error(ENOENT);
    }    
}

fn main() {
    // Expect a mountpoint as the first argument (after the binary name)
    let mountpoint = env::args().nth(1).expect("Usage: ./program <mountpoint>");

    // Initialize our custom filesystem (which connects to Telegram and spawns a cache updater)
    let fs = TelegramFS::init();

    // Mount the filesystem using FUSE (via fuser crate)
    fuser::mount2(
        fs, // our filesystem implementation
        mountpoint, // where to mount it in the system
        &[
            MountOption::RO, // Read-only filesystem
            MountOption::FSName("telegramfs".into()), // Filesystem name shown in system tools
            MountOption::AutoUnmount, // Auto-unmount on process exit
            MountOption::AllowOther,  // Allow users other than the mounter to access
        ],
    ).unwrap(); // Panic if mounting fails
}

/* helper functions */
fn get_file_extension(media: &Media) -> String {
    match media {
        Photo(_) => ".jpg".to_string(), // If the media is a photo, use .jpg extension
        Sticker(sticker) => get_mime_extension(sticker.document.mime_type()), // If it's a sticker, determine extension from MIME type (e.g., .webp)
        Document(document) => get_mime_extension(document.mime_type()), // If it's a document, extract extension from its MIME type
        Contact(_) => ".vcf".to_string(),  // If it's a contact (vCard), use .vcf extension
        _ => String::new(), // For all other media types, return an empty string (no extension)
    }
}

fn get_mime_extension(mime_type: Option<&str>) -> String {
    mime_type
        .and_then(|m| m.parse::<Mime>().ok()) // Try to parse the MIME type string into a `Mime` object
        .map(|mime| format!(".{}", mime.subtype())) // If successful, get the subtype (like "pdf", "jpeg", "mp4", etc.) and prepend a dot
        .unwrap_or(".bin".to_string())  // If parsing fails or MIME type is missing, default to ".bin"
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