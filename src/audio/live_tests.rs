//! Tests against the real PipeWire graph, using only null sinks (nothing is audible).
//! Run with `cargo test audio::live -- --ignored --test-threads=1`.

use std::time::Duration;

use super::{Engine, Route, pw};

const TEST_SINK: &str = "sonance-test";

/// Unloads what the test loaded even if it panics half-way.
struct Cleanup(Vec<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for m in &self.0 {
            let _ = std::process::Command::new("pactl").args(["unload-module", m]).stderr(std::process::Stdio::null()).status();
        }
    }
}

async fn with_test_sink<F: Future<Output = ()>>(f: impl FnOnce(Engine) -> F) {
    let module = pw::run(
        "pactl",
        &["load-module", "module-null-sink", &format!("sink_name={TEST_SINK}"), "sink_properties=device.description=SonanceTest"],
    )
    .await
    .unwrap();
    pw::wait_node(TEST_SINK, 3000).await.unwrap().expect("test sink");
    let mut cleanup = Cleanup(vec![module.trim().to_string()]);
    let engine = Engine::new();
    engine.ensure_sink().await.unwrap();
    cleanup.0.extend(engine.inner.state.lock().await.sink_module.clone());
    f(engine.clone()).await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_routes_and_measure() {
    with_test_sink(|e| async move {
        let err = e.measure("sonance", TEST_SINK, 1500).await.unwrap_err();
        println!("no route: {err:#}");
        assert!(err.to_string().contains("couldn't hear"));

        let route = |delay_ms, gain_db| [Route { sink: TEST_SINK.into(), delay_ms, gain_db, ..Default::default() }];
        e.set_routes(&route(0.0, 0.0)).await.unwrap();
        let base = e.measure("sonance", TEST_SINK, 2000).await.unwrap();
        println!("delay 0: {base:?}");
        for delay in [250.0, 1000.0] {
            e.set_routes(&route(delay, 0.0)).await.unwrap();
            let m = e.measure("sonance", TEST_SINK, 3000).await.unwrap();
            println!("delay {delay}: {m:?} (added {:.2} ms)", m.latency_ms - base.latency_ms);
            assert!((m.latency_ms - base.latency_ms - delay).abs() < 25.0);
        }
        let pid = e.inner.state.lock().await.routes[TEST_SINK].child_id();
        e.set_routes(&route(1000.0, -20.0)).await.unwrap();
        assert_eq!(e.inner.state.lock().await.routes[TEST_SINK].child_id(), pid, "gain change must not restart");
        let quiet = e.measure("sonance", TEST_SINK, 3000).await.unwrap();
        println!("-20 dB: {quiet:?}");
        assert!((quiet.level_db - base.level_db + 20.0).abs() < 1.5);

        e.set_routes(&[]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(pw::find_node("sonance-route-sonance_test-out").await.unwrap().is_none());
    })
    .await;
}

/// The sweep's band response through a route, flat and then with a live EQ change.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_eq_bands() {
    with_test_sink(|e| async move {
        let route = |eq_db: Vec<f32>| [Route { sink: TEST_SINK.into(), delay_ms: 100.0, eq_db, ..Default::default() }];
        e.set_routes(&route(Vec::new())).await.unwrap();
        let flat = e.measure("sonance", TEST_SINK, 2000).await.unwrap();
        println!("flat: {:?}", flat.bands_db);
        assert!(flat.bands_db.iter().all(|v| v.abs() < 0.5));

        let pid = e.inner.state.lock().await.routes[TEST_SINK].child_id();
        let eq = vec![3.0, -6.0, 0.0, 0.0, 0.0, 0.0, 2.0, -8.0];
        e.set_routes(&route(eq.clone())).await.unwrap();
        assert_eq!(e.inner.state.lock().await.routes[TEST_SINK].child_id(), pid, "EQ change must not restart");
        let m = e.measure("sonance", TEST_SINK, 2000).await.unwrap();
        println!("eq {eq:?}: {:?}", m.bands_db);
        // Octave averages blur the filters' peaks, so only the shape is checked.
        assert!(m.bands_db[0] > 1.0 && m.bands_db[1] < -3.0 && m.bands_db[6] > 0.5 && m.bands_db[7] < -4.0);
        assert!(m.bands_db[3..6].iter().all(|v| v.abs() < 1.0));

        e.set_routes(&route(Vec::new())).await.unwrap();
        let back = e.measure("sonance", TEST_SINK, 2000).await.unwrap();
        println!("flat again: {:?}", back.bands_db);
        assert!(back.bands_db.iter().all(|v| v.abs() < 0.5));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_wifi_stream() {
    let dir = std::env::var("SONANCE_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    with_test_sink(|e| async move {
        let urls = e.start_wifi_stream(18899).await.unwrap();
        println!("{urls:?}");
        // A tone into the (inaudible) Sonance sink, so the stream carries something.
        let mut tone = pw::command("pw-cat")
            .args(["--playback", "--target", "sonance", "-P", "{ node.dont-fallback=true node.dont-reconnect=true }"])
            .arg("/usr/share/sounds/freedesktop/stereo/complete.oga")
            .spawn()
            .ok();
        for ext in ["mp3", "flac", "wav"] {
            let url = format!("http://127.0.0.1:18899/sonance.{ext}");
            let head = pw::run("curl", &["-sI", &url]).await.unwrap();
            println!("HEAD {ext}:\n{head}");
            let out = format!("{dir}/stream.{ext}");
            let _ = pw::command("curl").args(["-s", "--max-time", "3", "-o", &out, &url]).status().await;
            let probe = pw::run("ffprobe", &["-v", "error", "-show_entries", "stream=codec_name,sample_rate,channels", "-of", "csv=p=0", &out])
                .await
                .unwrap();
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("{ext}: {size} bytes, {}", probe.trim());
            assert!(size > 10_000, "{ext} delivered too little");
        }
        assert!(pw::run("curl", &["-sI", "http://127.0.0.1:18899/nope"]).await.unwrap().contains("404"));
        if let Some(t) = tone.as_mut() {
            let _ = t.kill().await;
        }
        e.stop_wifi_stream().await;
        assert!(pw::run("curl", &["-s", "--max-time", "1", "http://127.0.0.1:18899/sonance.mp3"]).await.is_err());
    })
    .await;
}

/// Read-only: lists what BlueZ knows, connects to nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_bt_devices() {
    let e = Engine::new();
    for d in e.bt_devices().await.unwrap() {
        println!("{d:?}");
    }
    println!("mics: {:?}", e.mics().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_reap_stale() {
    pw::kill_stale().await;
}

/// The phone microphone end to end, inaudibly: a stand-in "phone" records the test sink at
/// 44.1 kHz and streams it over HTTPS to the phone page; two marked sweeps, the second
/// through a 250 ms delay line, must come out 250 ms apart.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_phone_take() {
    with_test_sink(|e| async move {
        let phone = crate::phone::Phone::start().await.unwrap();
        let url = phone.url.clone();
        let (base, query) = url.split_once("/?").unwrap();
        let (base, token) = (base.to_string(), query.trim_start_matches("t=").to_string());
        let client = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
        assert!(client.get(&url).send().await.unwrap().text().await.unwrap().contains("Sonance microphone"));
        assert_eq!(client.get(format!("{base}/state?t=wrong")).send().await.unwrap().status(), 403);

        let mut rec = tokio::process::Command::new("pw-cat")
            .args(["--record", "--raw", "--rate", "44100", "--channels", "1", "--format", "f32", "--target", TEST_SINK])
            .args(["-P", "{ stream.capture.sink=true }", "-"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut out = rec.stdout.take().unwrap();
        let streamer = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut offset, mut buf) = (0usize, vec![0u8; 4096 * 4]);
            loop {
                let mut got = 0;
                while got < buf.len() {
                    match out.read(&mut buf[got..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got += n,
                    }
                }
                let n = got / 4;
                let _ = client
                    .post(format!("{base}/chunk?t={token}&offset={offset}&rate=44100"))
                    .body(buf[..got].to_vec())
                    .send()
                    .await;
                offset += n;
            }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(phone.connected());
        phone.restart_take();

        let route = |delay_ms| [Route { sink: TEST_SINK.into(), delay_ms, ..Default::default() }];
        let mut sent = Vec::new();
        for delay in [0.0, 250.0] {
            e.set_routes(&route(delay)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            sent.push(e.play_marked("sonance").await.unwrap());
            tokio::time::sleep(Duration::from_millis(2500)).await;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (rate, take, origin) = phone.take().unwrap();
        streamer.abort();
        let a = crate::audio::analyse_remote(&take, rate, origin, sent[0], 1500).unwrap();
        let b = crate::audio::analyse_remote(&take, rate, origin, sent[1], 1500).unwrap();
        println!("rate {rate}, {} s; a {a:?}\nb {b:?}", take.len() / rate as usize);
        assert!((b.latency_ms - a.latency_ms - 250.0).abs() < 15.0, "{} vs {}", a.latency_ms, b.latency_ms);
        phone.stop();
    })
    .await;
}
