use dotenvy::dotenv;
use std::env;

mod bot;
use bot::{handle_update, Storage};

use frankenstein::client_ureq::Bot;
use frankenstein::methods::GetUpdatesParams;
use frankenstein::types::AllowedUpdate;
use frankenstein::TelegramApi;

fn main() {
    // load token
    dotenv().ok();
    let token = env::var("BOT_TOKEN").expect("BOT_TOKEN not set");

    // init bot and storage
    let bot = Bot::new(&token);
    let mut storage: Storage = Storage::new();
    let mut last_update_id: i64 = 0; // use i64 for offset

    loop {
        let update_params = GetUpdatesParams::builder()
            .offset(last_update_id + 1)
            .timeout(30)
            .allowed_updates(vec![AllowedUpdate::Message])
            .build();

        match bot.get_updates(&update_params) {
            Ok(response) => {
                for update in response.result {
                    // cast update_id to i64
                    last_update_id = update.update_id as i64;
                    handle_update(&bot, &mut storage, update);
                }
            }
            Err(error) => {
                eprintln!("error fetching updates: {:?}", error);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}