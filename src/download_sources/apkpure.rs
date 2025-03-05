use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::Path;
use std::rc::Rc;

use futures_util::StreamExt;
use indicatif::MultiProgress;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Url, Response};
use serde_json::json;
use tokio_dl_stream_to_disk::{AsyncDownload, error::ErrorKind as TDSTDErrorKind};
use tokio::time::{sleep, Duration as TokioDuration};

use crate::util::{OutputFormat, progress_bar::progress_wrapper};

fn http_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-cv", HeaderValue::from_static("3172501"));
    headers.insert("x-sv", HeaderValue::from_static("29"));
    headers.insert(
        "x-abis",
        HeaderValue::from_static("arm64-v8a,armeabi-v7a,armeabi"),
    );
    headers.insert("x-gp", HeaderValue::from_static("1"));
    headers
}

pub async fn download_apps(
    apps: Vec<(String, Option<String>)>,
    parallel: usize,
    sleep_duration: u64,
    outpath: &Path,
) {
    let mp = Rc::new(MultiProgress::new());
    let http_client = Rc::new(reqwest::Client::new());
    let headers = http_headers();
    let re = Rc::new(Regex::new(crate::consts::APKPURE_DOWNLOAD_URL_REGEX).unwrap());

    futures_util::stream::iter(
        apps.into_iter().map(|app| {
            let (app_id, app_version) = app;
            let http_client = Rc::clone(&http_client);
            let re = Rc::clone(&re);
            let headers = headers.clone();
            let mp = Rc::clone(&mp);
            let mp_log = Rc::clone(&mp);
            async move {
                let app_string = match app_version {
                    Some(ref version) => {
                        mp_log.suspend(|| println!("Downloading {} version {}...", app_id, version));
                        format!("{}@{}", app_id, version)
                    },
                    None => {
                        mp_log.suspend(|| println!("Downloading {}...", app_id));
                        app_id.to_string()
                    },
                };
                if sleep_duration > 0 {
                    sleep(TokioDuration::from_millis(sleep_duration)).await;
                }
                let versions_url = Url::parse(&format!("{}{}", crate::consts::APKPURE_VERSIONS_URL_FORMAT, app_id)).unwrap();
                let versions_response = http_client
                    .get(versions_url)
                    .headers(headers)
                    .send().await.unwrap();
                if let Some(app_version) = app_version {
                    let regex_string = format!("[[:^digit:]]{}:(?s:.)+?{}", regex::escape(&app_version), crate::consts::APKPURE_DOWNLOAD_URL_REGEX);
                    let re = Regex::new(&regex_string).unwrap();
                    download_from_response(versions_response, Box::new(Box::new(re)), app_string, outpath, mp).await;
                } else {
                    download_from_response(versions_response, Box::new(re), app_string, outpath, mp).await;
                }
            }
        })
    ).buffer_unordered(parallel).collect::<Vec<()>>().await;
}

async fn download_from_response(response: Response, re: Box<dyn Deref<Target=Regex>>, app_string: String, outpath: &Path, mp: Rc<MultiProgress>) {
    let mp_log = Rc::clone(&mp);
    let mp = Rc::clone(&mp);
    match response.status() {
        reqwest::StatusCode::OK => {
            let body = response.text().await.unwrap();
            match re.captures(&body) {
                Some(caps) if caps.len() >= 2 => {
                    let apk_xapk = caps.get(1).unwrap().as_str();
                    let download_url = caps.get(2).unwrap().as_str();
                    let fname = match apk_xapk {
                        "XAPKJ" => format!("{}.xapk", app_string),
                        _ => format!("{}.apk", app_string),
                    };

                    match AsyncDownload::new(download_url, Path::new(outpath), &fname).get().await {
                        Ok(mut dl) => {
                            let length = dl.length();
                            let cb = match length {
                                Some(length) => Some(progress_wrapper(mp)(fname.clone(), length)),
                                None => None,
                            };

                            match dl.download(&cb).await {
                                Ok(_) => mp_log.suspend(|| println!("{} downloaded successfully!", app_string)),
                                Err(err) if matches!(err.kind(), TDSTDErrorKind::FileExists) => {
                                    mp_log.println(format!("File already exists for {}. Skipping...", app_string)).unwrap();
                                },
                                Err(err) if matches!(err.kind(), TDSTDErrorKind::PermissionDenied) => {
                                    mp_log.println(format!("Permission denied when attempting to write file for {}. Skipping...", app_string)).unwrap();
                                },
                                Err(_) => {
                                    mp_log.println(format!("An error has occurred attempting to download {}.  Retry #1...", app_string)).unwrap();
                                    match AsyncDownload::new(download_url, Path::new(outpath), &fname).download(&cb).await {
                                        Ok(_) => mp_log.suspend(|| println!("{} downloaded successfully!", app_string)),
                                        Err(_) => {
                                            mp_log.println(format!("An error has occurred attempting to download {}.  Retry #2...", app_string)).unwrap();
                                            match AsyncDownload::new(download_url, Path::new(outpath), &fname).download(&cb).await {
                                                Ok(_) => mp_log.suspend(|| println!("{} downloaded successfully!", app_string)),
                                                Err(_) => {
                                                    mp_log.println(format!("An error has occurred attempting to download {}. Skipping...", app_string)).unwrap();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        Err(_) => {
                            mp_log.println(format!("Invalid response for {}. Skipping...", app_string)).unwrap();
                        }
                    }
                },
                _ => {
                    mp_log.println(format!("Could not get download URL for {}. Skipping...", app_string)).unwrap();
                }
            }

        },
        _ => {
            mp_log.println(format!("Invalid app response for {}. Skipping...", app_string)).unwrap();
        }
    }
}

pub async fn list_versions(apps: Vec<(String, Option<String>)>, options: HashMap<&str, &str>) {
    let http_client = Rc::new(reqwest::Client::new());
    let re = Rc::new(Regex::new(r"([[:alnum:]\.-]+):\([[:xdigit:]]{40,}").unwrap());
    let headers = http_headers();
    let output_format = match options.get("output_format") {
        Some(val) if val.to_lowercase() == "json" => OutputFormat::Json,
        _ => OutputFormat::Plaintext,
    };
    let json_root = Rc::new(RefCell::new(match output_format {
        OutputFormat::Json => Some(HashMap::new()),
        _ => None,
    }));

    for app in apps {
        let (app_id, _) = app;
        let http_client = Rc::clone(&http_client);
        let re = Rc::clone(&re);
        let json_root = Rc::clone(&json_root);
        let output_format = output_format.clone();
        let headers = headers.clone();
        async move {
            if output_format.is_plaintext() {
                println!("Versions available for {} on APKPure:", app_id);
            }
            let versions_url = Url::parse(&format!("{}{}", crate::consts::APKPURE_VERSIONS_URL_FORMAT, app_id)).unwrap();
            let versions_response = http_client
                .get(versions_url)
                .headers(headers)
                .send().await.unwrap();

            match versions_response.status() {
                reqwest::StatusCode::OK => {
                    let body = versions_response.text().await.unwrap();
                    let mut versions = HashSet::new();
                    for caps in re.captures_iter(&body) {
                        if caps.len() >= 2 {
                            versions.insert(caps.get(1).unwrap().as_str().to_string());
                        }
                    }
                    let mut versions = versions.drain().collect::<Vec<String>>();
                    versions.sort();
                    match output_format {
                        OutputFormat::Plaintext => {
                            println!("| {}", versions.join(", "));
                        },
                        OutputFormat::Json => {
                            let mut app_root: HashMap<String, Vec<HashMap<String, String>>> = HashMap::new();
                            app_root.insert("available_versions".to_string(), versions.into_iter().map(|v| {
                                let mut version_map = HashMap::new();
                                version_map.insert("version".to_string(), v);
                                version_map
                            }).collect());
                            json_root.borrow_mut().as_mut().unwrap().insert(app_id.to_string(), json!(app_root));
                        },
                    }
                }
                _ => {
                    match output_format {
                        OutputFormat::Plaintext => {
                            eprintln!("| Invalid app response for {}. Skipping...", app_id);
                        },
                        OutputFormat::Json => {
                            let mut app_root = HashMap::new();
                            app_root.insert("error".to_string(), "Invalid app response.".to_string());
                            json_root.borrow_mut().as_mut().unwrap().insert(app_id.to_string(), json!(app_root));
                        },
                    }
                }
            }
        }.await;
    }
    if output_format.is_json() {
        println!("{{\"source\":\"APKPure\",\"apps\":{}}}", json!(*json_root));
    };
}
