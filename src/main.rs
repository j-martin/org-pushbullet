extern crate keyring;
extern crate pb;
extern crate rayon;
extern crate readability;
extern crate regex;
extern crate time;
#[macro_use]
extern crate slugify;
extern crate url;

use pb::{PbAPI, PbError, Push, PushData};
use rayon::iter::ParallelIterator;
use rayon::prelude::*;
use readability::extractor;
use readability::extractor::Product;
use regex::Regex;
use slugify::slugify;
use std::env;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::fs;
use std::io::{BufReader, Write};
use std::io::prelude::*;
use std::iter::Iterator;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, ChildStdout};
use url::Url;

#[derive(Debug)]
pub enum ProcessingError {
    OtherSender,
    NotURL,
    FailedScraping,
    FailedParsing,
    Unexpected,
}

#[derive(Debug)]
struct ProcessedPush<'a> {
    push: &'a Push,
    file_path: PathBuf,
    title: String,
    url: String,
}

fn process_pushes<'a>(output_path: &Path, pushes: &'a Vec<Push>) -> Vec<ProcessedPush<'a>> {
    pushes
        .par_iter()
        .map(|p| process_push(&p, output_path))
        .filter(|p| p.is_ok())
        .map(|p| p.unwrap())
        .collect()
}

fn process_push<'a>(push: &'a Push, output_dir: &Path) -> Result<ProcessedPush<'a>, ProcessingError> {
    if push.sender_iden != push.receiver_iden {
        return Err(ProcessingError::OtherSender);
    }
    let url = match push.data {
        PushData::Link(Some(ref url)) => url,
        _ => {
            return Err(ProcessingError::NotURL);
        }
    };
    let url_str = url.to_string();
    let title = push.title.as_ref().unwrap_or(&url_str);
    println!("Processing: {}", title);
    let file_path = get_filename(output_dir, &title);
    let scrape = match panic::catch_unwind(|| { extractor::scrape(&url_str) }) {
        Ok(p) => p,
        Err(err) => {
            println!("Failed to fetch and/or parse page: {:?}", err);
            return Err(ProcessingError::FailedScraping);
        }
    };
    let product = match scrape {
        Ok(p) => p,
        Err(err) => {
            println!("Failed to fetch and/or parse page: {}", err.description());
            return Err(ProcessingError::FailedParsing);
        }
    };

    let mut file_obj = File::create(file_path.clone())
        .expect("Failed to create file.");

    let header = format_header(push, url, title);
    file_obj
        .write(header.as_bytes())
        .expect("Failed to write header.");

    let anchor_cleanup = Regex::new(r"(\*+ )(\[\[\#\S+\]\[\]\])")
        .expect("Failed to create regex.");

    let content = format_with_pandoc(&product);
    for line in content.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return Err(ProcessingError::Unexpected),
        };
        let result = anchor_cleanup.replace_all(&line, "${1}") + "\n";
        file_obj
            .write_all(result.as_bytes())
            .expect("Failed to write file content.");
    }
    Ok(ProcessedPush { push, file_path, title: title.to_string(), url: url_str.to_string() })
}

fn format_with_pandoc(product: &Product) -> BufReader<ChildStdout> {
    let process = Command::new("pandoc")
        .args(&["--columns", "100", "-f", "html", "-t", "org"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn pandoc.");

    process
        .stdin
        .expect("Could not access pandoc stdin.")
        .write_all(product.content.as_bytes())
        .expect("Could not pass content to pandoc.");

    BufReader::new(process.stdout.expect("Failed to read pandoc output."))
}

fn get_filename(output_dir: &Path, title: &str) -> PathBuf {
    let mut slug = slugify!(title);
    if slug.len() > 50 {
        slug = slug[..50].to_ascii_lowercase();
    }
    let filename: String = slug.to_owned() + ".org";
    output_dir.join(filename)
}

fn format_header(push: &Push, url: &Url, title: &String) -> String {
    let header = format!(
        "#+TITLE: {}\n\
         #+URL: {}\n\
         #+STARTUP: showeverything\n\
         #+PROCESSED_ON: {}\n\
         #+CREATED_AT: {}\n\
         #+MODIFIED_ON: {}\n\n",
        title,
        url,
        time::get_time().sec,
        push.created,
        push.modified,
    );
    header
}

fn format_entry(p: &ProcessedPush) -> String {
    format!("** [[file:{}/{}][{}]]\n\
        :PROPERTIES:\n\
        :URL: [[{}][url]]\n\
        :CREATED_AT: {}\n\
        :MODIFIED_ON: {}\n\
        :END:\n\n",
            p.file_path.parent().unwrap().to_str().unwrap(),
            p.file_path.file_name().unwrap().to_str().unwrap(),
            p.title,
            p.url,
            p.push.created,
            p.push.modified,
    )
}

fn fetch_pushes(since: f64) -> Vec<Push> {
    let token = match get_pb_token() {
        Ok(t) => t,
        Err(err) => panic!("Failed to get token. Make sure to set the token as an envvar 'PB_TOKEN', {}", err.description()),
    };
    let mut api: PbAPI = pb::PbAPI::new(&*token);
    let result: Result<(Vec<Push>, Option<String>), PbError> = api.loadn_since(100, since);
    let (pushes, cursor) = match result {
        Ok(p) => p,
        Err(err) => panic!("Failed to fetch pushes: {}", err),
    };
    match cursor {
        Some(c) => fetch_pushes_with_cursor(&mut api, since, c, pushes),
        _ => pushes,
    }
}

fn fetch_pushes_with_cursor<'a>(api: &mut PbAPI, since: f64, c: String, initial: Vec<Push>) -> Vec<Push> {
    let result: Result<(Vec<Push>, Option<String>), PbError> = api.loadn_from(100, c);
    let (mut pushes, cursor) = match result {
        Ok(a) => a,
        Err(why) => {
            println!("Error: {}", why.description());
            return initial;
        }
    };
    match cursor {
        Some(c) => {
            pushes.extend(initial);
            fetch_pushes_with_cursor(api, since, c, pushes)
        }
        _ => {
            pushes.extend(initial);
            pushes
        }
    }
}

fn get_pointer(pointer_path: &Path) -> f64 {
    if !pointer_path.exists() {
        0.0
    } else {
        let mut content = String::new();
        File::open(pointer_path)
            .expect("Failed to open pointer file. Using default.")
            .read_to_string(&mut content)
            .expect("Failed to read the pointer file.");
        content
            .replace("\n", "")
            .parse()
            .expect("Failed the file content to a f64.")
    }
}

fn get_pb_token() -> keyring::Result<String> {
    let service = "pushbullet";
    let username = "token";
    let keyring = keyring::Keyring::new(&service, &username);
    match env::var("PB_TOKEN") {
        Ok(v) => keyring.set_password(&v).expect("Failed to set token in the keyring"),
        Err(_) => (),
    };
    return keyring.get_password();
}

fn update_pointer(pointer_path: &PathBuf, processed_pushes: &Vec<ProcessedPush>) {
    processed_pushes
        .iter()
        .map(|p| p.push.modified as i64)
        .max()
        .map(|t| {
            let pointer = t as f64 + 1.0;
            File::create(pointer_path)
                .expect("Failed to open pointer file.")
                .write_all(pointer.to_string().as_bytes())
                .expect("Failed to write pointer.");
        });
}

fn update_entry_list(output_dir: &str, processed_pushes: &Vec<ProcessedPush>) {
    let output_list_path = PathBuf::from(&(output_dir.to_owned() + ".org"));
    let reference: Vec<String> = processed_pushes
        .iter()
        .map(|p| format_entry(&p))
        .collect();

    create_or_append_to_file(&output_list_path)
        .expect(&format!("Failed to create or open: {:?}", &output_list_path))
        .write_all(reference.join("\n").as_bytes())
        .expect("Failed to write entry to reference file.");
}

fn create_or_append_to_file(path: &PathBuf) -> std::io::Result<File> {
    if path.exists() {
        return OpenOptions::new()
            .write(true)
            .append(true)
            .open(&path);
    } else {
        return File::create(&path);
    }
}

fn main() {
    let output_dir = env::args()
        .nth(1)
        .unwrap_or("output".to_string());
    let output_dir = output_dir.trim_right_matches("/");

    let output_path = Path::new(&output_dir);
    let pointer_path = output_path.join(".pointer");

    fs::create_dir_all(output_path)
        .expect("Failed to create the output directory.");

    let pointer = get_pointer(&pointer_path);
    println!("Fetching pushes since: {}", pointer);

    let pushes = fetch_pushes(pointer);
    let processed_pushes = process_pushes(output_path, &pushes);

    update_entry_list(output_dir, &processed_pushes);
    update_pointer(&pointer_path, &processed_pushes);

    println!("Done: {} pushes processed successfully!", &processed_pushes.len());
}
