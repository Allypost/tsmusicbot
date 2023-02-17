extern crate audiopus;
extern crate byteorder;
extern crate serde;
extern crate serde_json;

use std::collections::VecDeque;
use std::process::{exit, Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Error, Result};
use audiopus::{Channels, SampleRate};
use byteorder::{BigEndian, ReadBytesExt};
use futures::prelude::*;
use log;
use serde::Deserialize;
use tokio;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};
use tsclientlib;
use tsclientlib::Connection;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};
use which::which;

use crate::audio_utils::MAX_OPUS_FRAME_SIZE;

mod audio_utils;

#[derive(Debug, Deserialize)]
struct Config {
    host: String,
    password: String,
    name: String,
    id: String,
}

#[derive(Debug)]
enum Action {
    PlayAudio(String),
    PlayNukedAudio(String),
    Stop,
    ChangeVolume { modifier: f32 },
    None,
}

#[derive(Debug)]
enum PlayTaskCmd {
    Stop,
    ChangeVolume { modifier: f32 },
}

#[derive(Debug)]
enum AudioPacket {
    Payload(OutPacket),
    None,
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            c.is_alphanumeric()
                || [' ', '.', ' ', '=', '\t', ',', '?', '!', ':', '&', '/', '_'].contains(c)
        })
        .collect()
}

fn parse_command(msg: &str) -> Action {
    let stripped = msg.replace("[URL]", "").replace("[/URL]", "");
    let sanitized = sanitize(&stripped).trim().to_string();

    if &sanitized[..=0] != "!" {
        return Action::None;
    }

    let split_vec: Vec<&str> = sanitized.split(' ').collect();

    if split_vec[0] == "!stop" {
        return Action::Stop;
    }

    if split_vec.len() < 2 {
        return Action::None;
    }

    if split_vec[0] == "!volume" {
        let amount = split_vec[1].parse::<u32>();
        return match amount {
            Err(_) => Action::None,
            Ok(num) => {
                let modifier: f32 = num.max(0).min(100) as f32 / 100_f32;
                Action::ChangeVolume { modifier }
            }
        };
    }

    if split_vec[0] == "!yt" {
        log::trace!("MSG: {}", split_vec[1]);
        return Action::PlayAudio(split_vec[1].to_string());
    }

    if split_vec[0] == "!brki" {
        log::trace!("MSG: {}", split_vec[1]);
        return Action::PlayNukedAudio(split_vec[1].to_string());
    }

    Action::None
}

const DEFAULT_VOLUME: f32 = 0.2;

fn get_audio_url(link: &String) -> Result<String> {
    log::debug!("Fetching audio url for `{}'", link);

    let ytdl_url = match Command::new("yt-dlp")
        .args(&[&link, "--extract-audio", "--get-url"])
        .stdout(Stdio::piped())
        .output()
    {
        Err(why) => {
            bail!("Failed to execute yt-dlp: {}", why);
        }
        Ok(process) => process,
    };

    let trimmed_urls = match String::from_utf8(ytdl_url.stdout) {
        Ok(urls) => {
            log::debug!("Got audio urls for `{}': {}", link, urls);
            urls.trim().to_string()
        }
        Err(why) => bail!("Empty yt-dlp command output: {}", why),
    };

    // Get first value after newline if it has newline
    let url = if trimmed_urls.contains('\n') {
        let s = trimmed_urls.split('\n').next().unwrap_or("");
        log::trace!(
            "Split out audio url for `{}' from {} into {}",
            link,
            trimmed_urls,
            s
        );
        s.to_string()
    } else {
        log::trace!(
            "Used given single audio url for `{}' into {}",
            link,
            trimmed_urls,
        );
        trimmed_urls
    };

    Ok(url)
}

async fn play_file(
    link: String,
    pkt_send: mpsc::Sender<AudioPacket>,
    mut cmd_recv: mpsc::Receiver<PlayTaskCmd>,
    volume: f32,
) {
    const FRAME_SIZE: usize = 960 * 2;
    const MAX_PACKET_SIZE: usize = 3 * MAX_OPUS_FRAME_SIZE;

    let codec = CodecType::OpusMusic;
    let mut current_volume = volume;

    let url = match get_audio_url(&link) {
        Ok(url) => url,
        Err(why) => {
            log::error!("Failed to get audio url: {}", why);
            return;
        }
    };

    if url.is_empty() {
        log::error!("Missing audio stream in {}", link);
        if let Err(e) = pkt_send.send(AudioPacket::None).await {
            log::error!("Status packet sending error: {}", e);
        }
        return;
    }

    const SAMPLE_RATE: SampleRate = SampleRate::Hz48000;
    const CHANNELS: Channels = Channels::Stereo;

    let encoder_res =
        audiopus::coder::Encoder::new(SAMPLE_RATE, CHANNELS, audiopus::Application::Audio);

    let encoder = match encoder_res {
        Ok(enc) => enc,
        Err(why) => {
            log::error!("Failed to create encoder: {}", why);
            return;
        }
    };

    log::trace!("Starting ffmpeg for {} with encoder {:?}", url, encoder);
    let ffmpeg = match Command::new("ffmpeg")
        .args(&[
            "-loglevel",
            "quiet",
            "-i",
            &url,
            "-af",
            &std::format!("aresample={}", SAMPLE_RATE as i32),
            "-f",
            "s16be",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(why) => panic!("couldn't spawn ffmpeg: {}", why),
        Ok(process) => process,
    };
    log::trace!("ffmpeg started");

    // let mut pcm_in_be: [i16; FRAME_SIZE] = [0; FRAME_SIZE];
    let mut pcm_in_be = [0 as i16; MAX_OPUS_FRAME_SIZE / (CHANNELS as usize)];
    let mut opus_pkt = [0 as u8; MAX_OPUS_FRAME_SIZE];

    let mut ffmpeg_stdout = ffmpeg.stdout.unwrap();

    log::info!("Starting playback for: {}", link);
    loop {
        let start = Instant::now();

        let cmd: Option<PlayTaskCmd> =
            match timeout(Duration::from_micros(1), cmd_recv.recv()).await {
                Err(_) => None,
                Ok(msg) => msg,
            };

        match cmd {
            None => {}
            Some(PlayTaskCmd::ChangeVolume { modifier }) => {
                current_volume = modifier;
            }
            Some(PlayTaskCmd::Stop) => {
                break;
            }
        };

        if let Err(e) = ffmpeg_stdout.read_i16_into::<BigEndian>(&mut pcm_in_be) {
            log::trace!("Failed to read from ffmpeg: {}", e);
            break;
        }

        for i in 0..MAX_OPUS_FRAME_SIZE {
            pcm_in_be[i] = (pcm_in_be[i] as f32 * current_volume) as i16;
        }
        let len = match encoder.encode(&pcm_in_be, &mut opus_pkt[..]) {
            Ok(x) => x,
            Err(e) => {
                log::error!("Failed to encode audio frame: {}", e);
                break;
            }
        };

        let packet = OutAudio::new(&AudioData::C2S {
            id: 0,
            codec,
            data: &opus_pkt[..len],
        });

        if let Err(e) = pkt_send.send(AudioPacket::Payload(packet)).await {
            log::error!("Audio packet sending error: {}", e);
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                log::error!("Status packet sending error: {}", e);
                return;
            }
            break;
        }

        let time_taken = start.elapsed();
        let ms_per_packet = Duration::from_millis(18);
        if time_taken < ms_per_packet {
            let sleep_time = ms_per_packet - time_taken;
            sleep(sleep_time).await;
        }
    }

    log::info!("Finished playing: {}", link);
    if let Err(e) = pkt_send.send(AudioPacket::None).await {
        log::error!("Status packet sending error: {}", e);
        return;
    }
    cmd_recv.close();
}

fn read_config() -> Result<Config> {
    let config_file = std::fs::File::open("config.json")?;
    match serde_json::from_reader(config_file) {
        Ok(config) => Ok(config),
        Err(e) => Err(Error::new(e)),
    }
}

async fn connect_to_teamspeak(config: Config) -> Result<tsclientlib::Connection> {
    let id = match tsclientlib::Identity::new_from_str(&config.id) {
        Ok(id) => id,
        Err(why) => {
            bail!("Could not parse identity: {:?}", why);
        }
    };

    let con_config = tsclientlib::Connection::build(config.host)
        .identity(id)
        .name(config.name)
        .password(config.password)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false);

    let mut conn = match con_config.connect() {
        Ok(con) => Ok(con),
        Err(e) => Err(Error::new(e)),
    }?;

    // Wait for connection to be established
    {
        let result = conn
            .events()
            .try_filter(|e| future::ready(matches!(e, tsclientlib::StreamItem::BookEvents(_))))
            .next()
            .await;

        if !result.is_some() {
            bail!("Could not connect to the server. Received no events.");
        }

        if let Err(why) = result.unwrap() {
            bail!("Could not connect to the server: {:?}", why);
        }
    }

    Ok(conn)
}

//noinspection RsTypeCheck
async fn disconnect(mut conn: tsclientlib::Connection, reason: &str) {
    log::debug!("Disconnecting for reason: `{}'", reason);

    let options = tsclientlib::DisconnectOptions::new()
        .reason(tsclientlib::Reason::Clientdisconnect)
        .message(reason);

    conn.disconnect(options).unwrap();

    conn.events().for_each(|_| future::ready(())).await;

    log::info!("Disconnected for reason: `{}'", reason);
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    real_main2().await
}

fn validate_required_commands_exist() {
    if which("ffmpeg").is_err() {
        log::error!("Unable to find ffmpeg");
        exit(-1);
    };

    if which("youtube-dl").is_err() {
        log::error!("Unable to find youtube-dl");
        exit(-1);
    };
}

async fn play_task(
    mut cmd_recv: mpsc::Receiver<PlayTaskCmd>,
    pkt_send: mpsc::Sender<AudioPacket>,
    conn: &mut Connection,
) -> Result<()> {
    let mut current_link = None;
    let mut current_task = None;

    let mut current_volume = 1.0;

    loop {
        let cmd: Option<PlayTaskCmd> =
            match timeout(Duration::from_micros(1), cmd_recv.recv()).await {
                Err(_) => None,
                Ok(msg) => msg,
            };

        match cmd {
            None => {}
            Some(PlayTaskCmd::ChangeVolume { modifier }) => {
                current_volume = modifier;
            }
            Some(PlayTaskCmd::Stop) => {
                break;
            }
        };

        if let Some(task) = &current_task {
            if let Err(e) = task {
                log::error!("Play task failed: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn real_main2() -> Result<()> {
    validate_required_commands_exist();

    let config = match read_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            log::error!("Failed to parse config: {}", e);
            exit(-1);
        }
    };

    let hostname = config.host.clone();

    log::debug!("Connecting to {}", hostname);
    let mut conn = connect_to_teamspeak(config).await?;
    log::info!("Connected to {}", hostname);

    loop {
        let event = conn
            .events()
            .try_filter(|e| future::ready(matches!(e, tsclientlib::StreamItem::BookEvents(_))))
            .next()
            .await;

        if event.is_none() {
            log::error!("Connection closed unexpectedly");
            break;
        }

        let tsclientlib::StreamItem::BookEvents(events) = event.unwrap()?;

        let (cmd_send, cmd_recv) = mpsc::channel(1);
        let (pkt_send, pkt_recv) = mpsc::channel(1);

        tokio::spawn(async move {
            play_task(cmd_recv, pkt_send, &mut conn).await;
        });

        let codec = CodecType::OpusMusic;
        let packet = OutAudio::new(&AudioData::C2S {
            id: 0,
            codec,
            data: &[],
        });
        if let Err(e) = conn.send_audio(packet) {
            log::warn!("Failed to send audio packet: {}", e);
            continue;
        }
        cmd_send
            .send("https://www.youtube.com/watch?v=5qap5aO4i9A")
            .await
            .unwrap();
    }

    disconnect(conn, "Disconnected by user").await;

    Ok(())
}

async fn real_main() -> Result<()> {
    if which("ffmpeg").is_err() {
        log::error!("Unable to find ffmpeg");
        exit(-1);
    };

    if which("youtube-dl").is_err() {
        log::error!("Unable to find youtube-dl");
        exit(-1);
    };

    let config = match read_config() {
        Ok(cfg) => cfg,
        Err(why) => {
            log::error!("Failed to parse config: {}", why);
            exit(-1);
        }
    };

    let hostname = config.host.clone();

    log::debug!("Connecting to {}", hostname);
    let mut conn = connect_to_teamspeak(config).await?;
    log::info!("Connected to {}", hostname);

    let (pkt_send, mut pkt_recv) = mpsc::channel(64);
    let (status_send, mut status_recv) = mpsc::channel(64);
    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<String> = VecDeque::new();
    let mut playing: bool = false;

    loop {
        let events = conn.events().try_for_each(|event| async {
            match event {
                tsclientlib::StreamItem::BookEvents(msg_vec) => {
                    for msg in msg_vec {
                        match msg {
                            tsclientlib::events::Event::Message {
                                invoker,
                                target: _,
                                message,
                            } => {
                                log::debug!("Message from {:?}: {}", invoker, message);

                                if let Err(e) = status_send.send(parse_command(&message)).await {
                                    log::error!("Status packet sending error: {}", e);
                                }
                            }

                            _ => {}
                        }
                    }
                }
                tsclientlib::StreamItem::NetworkStatsUpdated => {}
                evt => {
                    log::trace!("Event: {:?}", evt);
                }
            };

            Ok(())
        });

        tokio::select! {
            val = status_recv.recv() => {
                if let Some(action) = val {
                    match action {
                        Action::PlayAudio(link) | Action::PlayNukedAudio(link) => {
                            log::trace!("PlayAudio: {}", link);
                            if !playing {
                                playing = true;
                                let audio_task_pkt_send = pkt_send.clone();

                                let (task_cmd_send, task_cmd_recv) = mpsc::channel(4);

                                cmd_send = task_cmd_send;

                                tokio::spawn(async move {
                                    play_file(link, audio_task_pkt_send, task_cmd_recv, DEFAULT_VOLUME).await;
                                });
                            } else {
                                play_queue.push_back(link);
                            }
                        }
                        Action::ChangeVolume { modifier } => {
                            if playing {
                                let _ = cmd_send.send(PlayTaskCmd::ChangeVolume { modifier }).await;
                            };
                        }
                        Action::Stop => {
                            if playing {
                                let _ = cmd_send.send(PlayTaskCmd::Stop).await;
                            };
                        }
                        _ => {}
                    }
                }
            }

            val = pkt_recv.recv() => {
                if let Some(msg) = val {
                    if !playing {
                        continue;
                    }

                    match msg {
                        AudioPacket::Payload(pkt) => {
                            if let Err(e) = conn.send_audio(pkt) {
                                log::error!("Audio packet sending error: {:?}", e);
                                break;
                            }
                        },

                        AudioPacket::None => {
                            if play_queue.is_empty() {
                                playing = false;
                            } else {
                                let link = play_queue.pop_front().unwrap();
                                let audio_task_pkt_send = pkt_send.clone();

                                let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                cmd_send = task_cmd_send;

                                tokio::spawn(async move {
                                    play_file(link, audio_task_pkt_send, task_cmd_recv,  DEFAULT_VOLUME).await;
                                });
                            }
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                disconnect(conn, "Ctrl-C").await;
                break;
            }

            r = events => {
                r?;
                disconnect(conn, "Dunno, just leaving lmao").await;
                bail!("Disconnected");
            }
        }
    }

    Ok(())
}
