use core::fmt;
use std::fmt::Display;

use async_std::{
    fs::{File, OpenOptions},
    io::{prelude::BufReadExt, BufReader, WriteExt},
    path::Path,
    stream::StreamExt,
};
use chrono::{TimeZone, Utc};

const ENTRY_FILE_NAME: &str = "entries.log";

#[derive(Clone)]
pub struct TsEntry {
    pub url: String,
    pub sequence: u64,
    pub length: f64,
    pub size: u64,
    pub ts: i64,
    pub is_header: bool,
}

impl TsEntry {
    pub fn from(line: &str) -> Result<Self, String> {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 6 {
            return Err("Invalid input format: expected 6 fields separated by '|'".to_string());
        }
        Ok(TsEntry {
            url: parts[0].to_string(),
            sequence: parts[1]
                .parse()
                .map_err(|e| format!("Failed to parse sequence: {}", e))?,
            length: parts[2]
                .parse()
                .map_err(|e| format!("Failed to parse length: {}", e))?,
            size: parts[3]
                .parse()
                .map_err(|e| format!("Failed to parse size: {}", e))?,
            ts: parts[4]
                .parse()
                .map_err(|e| format!("Failed to parse timestamp: {}", e))?,
            is_header: parts[5]
                .parse()
                .map_err(|e| format!("Failed to parse is_header: {}", e))?,
        })
    }

    pub fn date_time(&self) -> String {
        let date_str = Utc.timestamp_opt(self.ts / 1000, 0).unwrap().to_rfc3339();
        format!("#EXT-X-PROGRAM-DATE-TIME:{}\n", date_str)
    }

    /// Convert entry into a segment in HLS manifest.
    /// If `continuous` is false, DISCONTINUITY and DATE-TIME will be added into tags, so that player can get precise video time for danmaku display.
    /// If `force_time` is true, DATE-TIME will be added into tags which ignores `continuous`.
    pub fn to_segment(&self, continuous: bool, force_time: bool) -> String {
        if self.is_header {
            return "".into();
        }

        let mut content = if continuous {
            String::new()
        } else {
            "#EXT-X-DISCONTINUITY\n".into()
        };

        if !continuous || force_time {
            content += &self.date_time();
        }
        content += &format!("#EXTINF:{:.2},\n", self.length);
        content += &format!("{}\n", self.url);

        content
    }
}

impl Display for TsEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{}|{}|{}|{}|{}|{}",
            self.url, self.sequence, self.length, self.size, self.ts, self.is_header
        )
    }
}

/// EntryStore is used to management stream segments, which is basicly a simple version of hls manifest,
/// and of course, provids methods to generate hls manifest for frontend player.
pub struct EntryStore {
    // append only log file
    log_file: File,
    header: Option<TsEntry>,
    entries: Vec<TsEntry>,
    total_duration: f64,
    total_size: u64,
    last_sequence: u64,

    pub continue_sequence: u64,
}

impl EntryStore {
    pub async fn new(work_dir: &str) -> Self {
        // if work_dir is not exists, create it
        if !Path::new(work_dir).exists().await {
            std::fs::create_dir_all(work_dir).unwrap();
        }
        // open append only log file
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/{}", work_dir, ENTRY_FILE_NAME))
            .await
            .unwrap();
        let mut entry_store = Self {
            log_file,
            header: None,
            entries: vec![],
            total_duration: 0.0,
            total_size: 0,
            last_sequence: 0,
            continue_sequence: 0,
        };

        entry_store.load(work_dir).await;

        entry_store
    }

    async fn load(&mut self, work_dir: &str) {
        let file = OpenOptions::new()
            .create(false)
            .read(true)
            .open(format!("{}/{}", work_dir, ENTRY_FILE_NAME))
            .await
            .unwrap();
        let mut lines = BufReader::new(file).lines();
        while let Some(Ok(line)) = lines.next().await {
            let entry = TsEntry::from(&line);
            if let Err(e) = entry {
                log::error!("Failed to parse entry: {} {}", e, line);
                continue;
            }

            let entry = entry.unwrap();

            if entry.sequence > self.last_sequence {
                self.last_sequence = entry.sequence;
            }

            if entry.is_header {
                self.header = Some(entry.clone());
            } else {
                self.entries.push(entry.clone());
            }

            self.total_duration += entry.length;
            self.total_size += entry.size;
        }

        self.continue_sequence = self.last_sequence + 100;
    }

    pub async fn add_entry(&mut self, entry: TsEntry) {
        if entry.is_header {
            self.header = Some(entry.clone());
        } else {
            self.entries.push(entry.clone());
        }

        if let Err(e) = self.log_file.write_all(entry.to_string().as_bytes()).await {
            log::error!("Failed to write entry to log file: {}", e);
        }

        self.log_file.flush().await.unwrap();

        if self.last_sequence < entry.sequence {
            self.last_sequence = entry.sequence;
        }

        self.total_duration += entry.length;
        self.total_size += entry.size;
    }

    pub fn get_header(&self) -> Option<&TsEntry> {
        self.header.as_ref()
    }

    pub fn total_duration(&self) -> f64 {
        self.total_duration
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn last_ts(&self) -> Option<i64> {
        self.entries.last().map(|entry| entry.ts)
    }

    pub fn first_ts(&self) -> Option<i64> {
        self.entries.first().map(|e| e.ts)
    }

    /// Generate a hls manifest for selected range.
    /// `vod` indicates the manifest is for stream or video.
    /// `force_time` adds DATE-TIME tag for each entry.
    pub fn manifest(&self, vod: bool, force_time: bool, range: Option<Range>) -> String {
        let mut m3u8_content = "#EXTM3U\n".to_string();
        m3u8_content += "#EXT-X-VERSION:6\n";
        m3u8_content += if vod {
            "#EXT-X-PLAYLIST-TYPE:VOD\n"
        } else {
            "#EXT-X-PLAYLIST-TYPE:EVENT\n"
        };
        let end_content = if vod { "#EXT-X-ENDLIST" } else { "" };

        if self.entries.is_empty() {
            m3u8_content += end_content;
            return m3u8_content;
        }

        m3u8_content += &format!(
            "#EXT-X-TARGETDURATION:{}\n",
            (0.5 + self.entries.first().unwrap().length).floor()
        );

        // add header, FMP4 need this
        if let Some(header) = &self.header {
            m3u8_content += &format!("#EXT-X-MAP:URI=\"{}\"\n", header.url);
        }

        let first_entry = self.entries.first().unwrap();
        let first_entry_ts = first_entry.ts / 1000;
        let mut previous_seq = first_entry.sequence;
        for e in &self.entries {
            // ignore header, cause it's already in EXT-X-MAP
            if e.is_header {
                continue;
            }
            let discontinuous = e.sequence < previous_seq || e.sequence - previous_seq > 1;
            previous_seq = e.sequence;

            let entry_offset = (e.ts / 1000 - first_entry_ts) as f32;
            if range.is_none_or(|r| r.is_in(entry_offset)) {
                m3u8_content += &e.to_segment(!discontinuous, force_time);
            }
        }

        m3u8_content += end_content;
        m3u8_content
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub x: f32,
    pub y: f32,
}

impl Range {
    pub fn is_in(&self, v: f32) -> bool {
        v >= self.x && v <= self.y
    }
}
