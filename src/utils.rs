/*
 * Rust Query API - A versatile API facade for the Hypixel Auction API
 * Copyright (c) 2021 kr45732
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use crate::{statics::*, structs::*};
use chrono::prelude::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use futures::{pin_mut, Future};
use log::{error, info};
use postgres_types::{ToSql, Type};
use serde_json::Value;
use std::{fs::OpenOptions, result::Result as StdResult, thread, time::SystemTime};
use tokio::time::{self, Duration};
use tokio_postgres::{binary_copy::BinaryCopyInWriter, Error};

/* Repeat a task */
pub async fn start_auction_loop<F, Fut>(mut f: F)
where
    F: Send + 'static + FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Create stream of intervals.
    let mut interval = time::interval(get_duration_until_api_update().await);
    tokio::spawn(async move {
        loop {
            // Skip tick at 0ms
            interval.tick().await;
            // Wait until next tick.
            interval.tick().await;
            // Spawn a task for this tick.
            f().await;
            // Updated to new interval
            interval = time::interval(get_duration_until_api_update().await);
        }
    });
}

/* Gets the time until the next API update according to Cloudflare headers */
async fn get_duration_until_api_update() -> Duration {
    let mut num_attempts = 0;
    loop {
        num_attempts += 1;
        let res = HTTP_CLIENT
            .get("https://api.hypixel.net/skyblock/auctions?page=0")
            .send()
            .await;
        match res {
            Ok(res_unwrap) => match res_unwrap.headers().get("age") {
                Some(age_header) => {
                    let age = age_header.to_str().unwrap().parse::<u64>().unwrap();

                    let max_age_header = res_unwrap.headers().get("cache-control").unwrap();
                    let mut max_age_split = max_age_header.to_str().unwrap().split("s-maxage=");
                    max_age_split.next();
                    let max_age = max_age_split.next().unwrap_or("60").parse::<u64>().unwrap();

                    // Cloudfare doesn't return an exact time in ms, so the +2 accounts for that
                    let time = max_age - age + 2;

                    println!("Max Age: {}, Age: {}, Time: {}", max_age, age, time); // Debugging purposes

                    if time == 0 {
                        return Duration::from_millis(1);
                    }

                    return Duration::from_secs(max_age - age + 2);
                }
                None => return Duration::from_millis(1),
            },
            Err(_) => {
                // Retry in 15 seconds
                thread::sleep(Duration::from_secs(15));
            }
        }
        if num_attempts == 15 {
            panic("Failed 15 consecutive attempts to contact the Hypixel API".to_string()).await;
        }
    }
}

/* Log and send an info message to the Discord webhook */
pub async fn info(desc: String) {
    info!("{}", desc);
    unsafe {
        let _ = WEBHOOK
            .as_ref()
            .unwrap()
            .send(|message| {
                message.embed(|embed| {
                    embed
                        .title("Information")
                        .url(&format!("http://{}", &URL.lock().unwrap()).to_string())
                        .color(0x00FFFF)
                        .description(&desc)
                        .timestamp(&get_discord_timestamp())
                })
            })
            .await;
    }
}

/* Log and send an error message to the Discord webhook */
pub async fn error(desc: String) {
    error!("{}", desc);
    unsafe {
        let _ = WEBHOOK
            .as_ref()
            .unwrap()
            .send(|message| {
                message.embed(|embed| {
                    embed
                        .title("Error")
                        .url(&format!("http://{}", &URL.lock().unwrap()).to_string())
                        .color(0xFF0000)
                        .description(&desc)
                        .timestamp(&get_discord_timestamp())
                })
            })
            .await;
    }
}

/* Send a panic message to the Discord webhook and panic */
pub async fn panic(desc: String) {
    error!("{}", desc);
    unsafe {
        let _ = WEBHOOK
            .as_ref()
            .unwrap()
            .send(|message| {
                message.embed(|embed| {
                    embed
                        .title("Force panic")
                        .url(&format!("http://{}", &URL.lock().unwrap()).to_string())
                        .color(0xFF0000)
                        .description(&desc)
                        .timestamp(&get_discord_timestamp())
                })
            })
            .await;
    }
    panic!("{}", desc);
}

/* Forms the current timestamp for a Discord Embed */
fn get_discord_timestamp() -> String {
    let dt: DateTime<Utc> = SystemTime::now().into();
    format!("{}", dt.format("%+"))
}

pub fn to_nbt(item_bytes: ItemBytes) -> Result<PartialNbt, Box<dyn std::error::Error>> {
    let bytes: StdResult<Vec<u8>, _> = item_bytes.into();
    let nbt: PartialNbt = nbt::from_gzip_reader(std::io::Cursor::new(bytes?))?;
    Ok(nbt)
}

pub fn calculate_with_taxes(price: i64) -> i64 {
    let price_float = price as f64;
    let tax_rate = if price >= 1000000 { 0.98 } else { 0.99 };

    return (price_float * tax_rate) as i64;
}

pub fn valid_api_key(key: String, admin_only: bool) -> bool {
    let admin_api_key = ADMIN_API_KEY.lock().unwrap().to_owned();
    if admin_only {
        return admin_api_key.is_empty() || admin_api_key == key;
    }

    let api_key = API_KEY.lock().unwrap().to_owned();
    return api_key.is_empty()
        || api_key == key
        || admin_api_key.is_empty()
        || admin_api_key == key;
}

pub fn update_lower_else_insert(id: &String, starting_bid: i64, prices: &mut DashMap<String, i64>) {
    if let Some(mut ele) = prices.get_mut(id) {
        if starting_bid < *ele {
            *ele = starting_bid;
        }
        return;
    }

    prices.insert(id.clone(), starting_bid);
}

pub async fn update_query_database(auctions: Vec<DatabaseItem>) -> Result<u64, Error> {
    unsafe {
        let database = DATABASE.as_ref().unwrap();
        let _ = database.simple_query("TRUNCATE TABLE query").await;

        let copy_statement = database
            .prepare("COPY query FROM STDIN BINARY")
            .await
            .unwrap();
        let copy_sink = database.copy_in(&copy_statement).await.unwrap();

        let copy_writer = BinaryCopyInWriter::new(
            copy_sink,
            &[
                Type::TEXT,
                Type::TEXT,
                Type::INT8,
                Type::TEXT,
                Type::TEXT,
                Type::TEXT,
                Type::INT8,
                Type::TEXT_ARRAY,
                Type::BOOL,
                BID_ARRAY.to_owned().unwrap(),
            ],
        );

        pin_mut!(copy_writer);

        // Write to copy sink
        for m in &auctions {
            let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
            row.push(&m.uuid);
            row.push(&m.auctioneer);
            row.push(&m.end_t);
            row.push(&m.item_name);
            row.push(&m.tier);
            row.push(&m.item_id);
            row.push(&m.starting_bid);
            row.push(&m.enchants);
            row.push(&m.bin);
            row.push(&m.bids);

            copy_writer.as_mut().write(&row).await.unwrap();
        }

        copy_writer.finish().await
    }
}

pub async fn update_pets_database(pet_prices: &mut DashMap<String, i64>) -> Result<u64, Error> {
    unsafe {
        let database = DATABASE.as_ref().unwrap();

        // Add all old pet prices to the new prices if the new prices doesn't have that old pet name
        let old_pet_prices = database.query("SELECT * FROM pets", &[]).await?;
        for old_price in old_pet_prices {
            let old_price_name: String = old_price.get("name");
            let mut new_has = false;
            for new_price in pet_prices.iter_mut() {
                if old_price_name == *new_price.key() {
                    new_has = true;
                    break;
                }
            }
            if !new_has {
                pet_prices.insert(old_price_name, old_price.get("price"));
            }
        }

        let _ = database.simple_query("TRUNCATE TABLE pets").await;

        let copy_statement = database
            .prepare("COPY pets FROM STDIN BINARY")
            .await
            .unwrap();
        let copy_sink = database.copy_in(&copy_statement).await.unwrap();
        let copy_writer = BinaryCopyInWriter::new(copy_sink, &[Type::TEXT, Type::INT8]);
        pin_mut!(copy_writer);

        // Write to copy sink
        for m in pet_prices.iter() {
            copy_writer
                .as_mut()
                .write(&[
                    m.key() as &(dyn ToSql + Sync),
                    m.value() as &(dyn ToSql + Sync),
                ])
                .await
                .unwrap();
        }

        copy_writer.finish().await
    }
}

pub async fn update_avg_ah_database(avg_ah_prices: Vec<AvgAh>, time_t: i64) -> Result<u64, Error> {
    unsafe {
        let database = DATABASE.as_ref().unwrap();

        // Delete auctions older than 5 days
        let _ = database
            .simple_query(
                &format!(
                    "DELETE FROM average WHERE time_t < {}",
                    (Utc::now() - chrono::Duration::days(5)).timestamp_millis()
                )
                .to_string(),
            )
            .await;

        // Insert new average auctions
        database
            .execute(
                "INSERT INTO average VALUES ($1, $2)",
                &[&time_t, &avg_ah_prices],
            )
            .await
    }
}

pub async fn update_bins_local(bin_prices: &DashMap<String, i64>) -> Result<(), serde_json::Error> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("lowestbin.json")
        .unwrap();
    serde_json::to_writer(file, bin_prices)
}

pub async fn update_under_bins_local(bin_prices: &Vec<Value>) -> Result<(), serde_json::Error> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("underbin.json")
        .unwrap();
    serde_json::to_writer(file, bin_prices)
}

pub async fn update_query_items_local(query_items: DashSet<String>) {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("query_items.json")
        .unwrap();
    let _ = serde_json::to_writer(file, &query_items);
}
