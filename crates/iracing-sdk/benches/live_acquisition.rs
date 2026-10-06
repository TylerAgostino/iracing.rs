//! Simulator-backed owned-frame acquisition diagnostic. Waits occur outside timing.
//! Run with `cargo bench -p iracing-sdk --features benchmark --bench live-acquisition-diagnostic -- --profile LABEL`.

#[cfg(windows)]
#[path = "support/live.rs"]
mod live;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, anyhow, bail};
    use iracing_sdk::provider::VariableHeadersProvider;
    use iracing_sdk::windows::WaitResult;
    use iracing_sdk::{VariableSchema, WindowsConnection};
    use serde_json::json;
    use std::{hint::black_box, time::Instant};

    let options = live::Options::parse()?;
    let mut connection = WindowsConnection::try_connect()
        .context("iRacing shared memory is unavailable; start an active session")?;
    if !connection.is_connected() {
        bail!("iRacing has no active session");
    }

    let header = connection.header_snapshot()?;
    let tick_hz = header.tick_rate;
    if tick_hz <= 0 {
        bail!("invalid source tick rate");
    }
    let frame_size = usize::try_from(header.buffer_length)?;
    if frame_size == 0 {
        bail!("invalid zero frame size");
    }
    let initial_session_version = header.session_info_update;
    let headers = connection.variable_headers()?;
    let schema = VariableSchema::from_headers(&headers, frame_size)?;
    if schema.variables.is_empty() {
        bail!("live session has no telemetry variable schema");
    }
    let initial_fingerprint = live::fingerprint(&schema)?;
    let mut run = live::new_run(&options, &schema, tick_hz)?;

    let target = options.warmup_frames + options.target_frames;
    let deadline = Instant::now() + options.timeout;
    let mut accepted = 0_usize;
    let mut attempted = 0_usize;
    let mut no_frame_polls = 0_usize;
    let mut wait_signals = 0_usize;
    let mut wait_timeouts = 0_usize;
    let mut copied_bytes = 0_u64;
    let mut skipped_ticks = 0_u64;
    let mut previous_tick = None;
    let mut samples = Vec::with_capacity(options.target_frames);
    let mut sampling_started = None;
    let mut failure = None;

    while accepted < target && Instant::now() < deadline {
        if !connection.is_connected() {
            failure = Some("simulator disconnected");
            break;
        }
        match connection.wait_for_update(std::time::Duration::from_millis(100))? {
            WaitResult::Signaled => wait_signals += 1,
            WaitResult::Timeout => wait_timeouts += 1,
        }

        let start = Instant::now();
        let result = connection.get_new_data()?;
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        attempted += 1;

        let Some(frame) = result else {
            no_frame_polls += 1;
            continue;
        };
        let tick = frame.tick;
        let session_version = frame.session_info_update;
        let frame_len = frame.data.len();

        if frame_len != frame_size {
            failure = Some("frame geometry changed");
            break;
        }
        if session_version != initial_session_version {
            failure = Some("session version changed");
            break;
        }

        let current_header = connection.header_snapshot()?;
        if current_header.buffer_length != header.buffer_length || current_header.tick_rate != tick_hz {
            failure = Some("frame geometry or source rate changed");
            break;
        }
        let current_headers = connection.variable_headers()?;
        let current_schema = VariableSchema::from_headers(&current_headers, frame_size)?;
        if live::fingerprint(&current_schema)? != initial_fingerprint {
            failure = Some("schema changed");
            break;
        }

        black_box(frame);
        accepted += 1;
        if accepted > options.warmup_frames {
            if sampling_started.is_none() {
                sampling_started = Some(Instant::now());
            }
            samples.push(elapsed_ns);
            copied_bytes += frame_len as u64;
            if let Some(previous) = previous_tick {
                let advance = tick.wrapping_sub(previous);
                skipped_ticks += u64::try_from(advance.saturating_sub(1)).unwrap_or(0);
            }
            previous_tick = Some(tick);
        }
    }

    let complete = failure.is_none() && samples.len() == options.target_frames;
    let elapsed_s = sampling_started.map_or(0.0, |start| start.elapsed().as_secs_f64());
    let (p50, p95, p99) = if samples.is_empty() {
        (None, None, None)
    } else {
        (
            Some(live::percentile(&mut samples.clone(), 0.50)),
            Some(live::percentile(&mut samples.clone(), 0.95)),
            Some(live::percentile(&mut samples, 0.99)),
        )
    };
    let raw_samples_ns = samples.clone();
    run.cases.push(live::Case {
        id: "live_acquisition/owned_frame",
        experiment_version: 2,
        status: if complete { "complete" } else { "incomplete" },
        samples: accepted.saturating_sub(options.warmup_frames),
        parameters: json!({"timed_boundary": "get_new_data/live_frame_snapshot"}),
        metrics: json!({
            "p50_us": p50, "p95_us": p95, "p99_us": p99,
            "attempted": attempted, "accepted_total": accepted,
            "accepted_frames": accepted.saturating_sub(options.warmup_frames),
            "no_frame_polls": no_frame_polls, "wait_signals": wait_signals,
            "wait_timeouts": wait_timeouts, "copied_bytes": copied_bytes,
            "samples_ns": raw_samples_ns,
            "elapsed_s": elapsed_s, "effective_hz": if elapsed_s > 0.0 { Some(samples.len() as f64 / elapsed_s) } else { None },
            "skipped_ticks": skipped_ticks, "failure": failure,
        }),
    });
    let path = live::write_run(&run, &options)?;
    println!("live acquisition report: {}", path.display());
    if !complete {
        return Err(anyhow!(
            "capture incomplete: {}",
            failure.unwrap_or("timeout or insufficient frames")
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("live-acquisition-diagnostic requires Windows and an active iRacing session");
}
