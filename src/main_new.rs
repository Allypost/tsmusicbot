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

pub async fn main() -> Result<()> {
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
