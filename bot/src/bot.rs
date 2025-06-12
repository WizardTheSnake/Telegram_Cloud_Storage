use frankenstein::client_ureq::Bot;
use frankenstein::methods::{SendMessageParams, SendDocumentParams};
use frankenstein::input_file::FileUpload;
use frankenstein::updates::{Update, UpdateContent};
use frankenstein::TelegramApi;
use std::collections::HashMap;

// basic info about a file sent by user
pub struct FileInfo {
    pub file_id: String,
    pub file_name: String,
}

// storage keeps files per chat
pub type Storage = HashMap<i64, Vec<FileInfo>>;

pub fn handle_update(bot: &Bot, storage: &mut Storage, update: Update) {
    if let UpdateContent::Message(message) = update.content {
        let chat_id = message.chat.id;
        if let Some(document) = message.document {
            // file upload detected
            let file_id = document.file_id.clone(); // use file_id field
            let file_name = document.file_name.clone().unwrap_or_else(|| "<unknown>".to_string());

            let user_files = storage.entry(chat_id).or_insert_with(Vec::new);
            user_files.push(FileInfo {
                file_id: file_id.clone(),
                file_name: file_name.clone(),
            });

            let confirmation = format!("uploaded file: {}", file_name);
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(&confirmation)
                .build();
            bot.send_message(&params).ok();
        } else if let Some(text) = message.text {
            // basic command handling
            if text.starts_with("/start") || text.starts_with("/help") {
                let welcome = "yo! this is your cloud bot.\nsend files as documents to upload.\nuse /list to list them.\n/get <number> to download.";
                let params = SendMessageParams::builder()
                    .chat_id(chat_id)
                    .text(welcome)
                    .build();
                bot.send_message(&params).ok();
            } else if text.starts_with("/list") {
                // list all uploaded files
                let files_list = storage.get(&chat_id);
                let response_text = if let Some(files) = files_list {
                    if files.is_empty() {
                        "no files yet.".to_string()
                    } else {
                        let mut list_text = String::from("your files:\n");
                        for (index, file) in files.iter().enumerate() {
                            list_text.push_str(&format!("{}: {}\n", index + 1, file.file_name));
                        }
                        list_text
                    }
                } else {
                    "no files yet.".to_string()
                };
                let params = SendMessageParams::builder()
                    .chat_id(chat_id)
                    .text(&response_text)
                    .build();
                bot.send_message(&params).ok();
            } else if text.starts_with("/get") {
                // handle file download
                let parts: Vec<&str> = text.split_whitespace().collect();
                if parts.len() < 2 {
                    let params = SendMessageParams::builder()
                        .chat_id(chat_id)
                        .text("usage: /get <file_number>")
                        .build();
                    bot.send_message(&params).ok();
                } else if let Ok(index) = parts[1].parse::<usize>() {
                    if let Some(files) = storage.get(&chat_id) {
                        if index >= 1 && index <= files.len() {
                            let file = &files[index - 1];
                            let send_doc_params = SendDocumentParams::builder()
                                .chat_id(chat_id)
                                .document(FileUpload::String(file.file_id.clone()))
                                .build();
                            bot.send_document(&send_doc_params).ok();
                        } else {
                            let params = SendMessageParams::builder()
                                .chat_id(chat_id)
                                .text("invalid file index.")
                                .build();
                            bot.send_message(&params).ok();
                        }
                    }
                } else {
                    let params = SendMessageParams::builder()
                        .chat_id(chat_id)
                        .text("invalid number format.")
                        .build();
                    bot.send_message(&params).ok();
                }
            } else {
                // fallback for unknown input
                let params = SendMessageParams::builder()
                    .chat_id(chat_id)
                    .text("dunno what u mean. try /list or /get.")
                    .build();
                bot.send_message(&params).ok();
            }
        }
    }
}