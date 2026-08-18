use std::error::Error;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;

use assert_matches::assert_matches;
use axum::extract::Request;
use axum::response::IntoResponse;
use memmap2::MmapOptions;
use reqwest::header::HeaderMap;
use reqwest::{header, Client, Method, StatusCode, Url};
use rstest::rstest;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::timeout;
use tokio_stream::wrappers::WatchStream;

use crate::sparse_range::SparseRange;
use crate::{
    run_streamer, AsyncHttpRangeReader, AsyncHttpRangeReaderError, CheckSupportMethod, Inner,
    SharedMemoryMap, StreamerState,
};

/// Serve `bytes=0-3` and `bytes=-4`, returning HTTP 500 for other ranges.
async fn spawn_failing_range_server() -> Result<Url, Box<dyn Error>> {
    let app = axum::Router::new().fallback(|request: Request| async move {
        match *request.method() {
            Method::HEAD => (
                StatusCode::OK,
                [
                    (header::CONTENT_LENGTH, "16"),
                    (header::ACCEPT_RANGES, "bytes"),
                ],
            )
                .into_response(),
            Method::GET => {
                match request
                    .headers()
                    .get(header::RANGE)
                    .and_then(|value| value.to_str().ok())
                {
                    Some("bytes=0-3") => (
                        StatusCode::PARTIAL_CONTENT,
                        [(header::CONTENT_RANGE, "bytes 0-3/16")],
                        &b"safe"[..],
                    )
                        .into_response(),
                    Some("bytes=-4") => (
                        StatusCode::PARTIAL_CONTENT,
                        [(header::CONTENT_RANGE, "bytes 12-15/16")],
                        &b"89ab"[..],
                    )
                        .into_response(),
                    _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/file", listener.local_addr()?))?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    Ok(url)
}

#[rstest]
#[case(CheckSupportMethod::Head)]
#[case(CheckSupportMethod::NegativeRangeRequest(4))]
#[tokio::test]
async fn cached_range_survives_failed_prefetch(
    #[case] check_method: CheckSupportMethod,
) -> Result<(), Box<dyn Error>> {
    let url = spawn_failing_range_server().await?;
    let (mut reader, _) = AsyncHttpRangeReader::new(
        Client::builder().no_proxy().build()?,
        url,
        check_method,
        HeaderMap::new(),
    )
    .await?;

    let mut initial = [0; 4];
    reader.read_exact(&mut initial).await?;
    assert_eq!(&initial, b"safe");

    // Wait for the failing download task to terminate without consuming its watch update.
    let request_tx = reader.inner.get_mut().request_tx.clone();
    reader.prefetch(8..12).await;
    timeout(Duration::from_secs(5), request_tx.closed()).await?;

    reader.seek(SeekFrom::Start(0)).await?;
    let mut repeated = [0; 4];
    reader.read_exact(&mut repeated).await?;
    assert_eq!(&repeated, b"safe");

    // A missing range still reports the original HTTP failure.
    reader.seek(SeekFrom::Start(8)).await?;
    let error = reader
        .read_exact(&mut repeated)
        .await
        .expect_err("the failed range must not become readable");
    assert_matches!(
        error.get_ref().and_then(|error| error.downcast_ref::<AsyncHttpRangeReaderError>()),
        Some(AsyncHttpRangeReaderError::IoError(source))
            if source
                .get_ref()
                .and_then(|error| error.downcast_ref::<reqwest_middleware::Error>())
                .and_then(reqwest_middleware::Error::status)
                == Some(StatusCode::INTERNAL_SERVER_ERROR)
    );
    Ok(())
}

#[tokio::test]
async fn cached_range_survives_streamer_cancellation() -> Result<(), Box<dyn Error>> {
    let url = spawn_failing_range_server().await?;
    let memory_map = Arc::new(SharedMemoryMap::new(MmapOptions::new().len(16).map_anon()?));
    let weak_mapping = Arc::downgrade(&memory_map);
    let (request_tx, request_rx) = mpsc::channel(10);
    let (state_tx, state_rx) = watch::channel(StreamerState::default());
    let streamer = tokio::spawn(run_streamer(
        Client::builder().no_proxy().build()?.into(),
        url,
        HeaderMap::new(),
        None,
        Arc::clone(&memory_map),
        state_tx,
        request_rx,
    ));
    let mut reader = AsyncHttpRangeReader {
        len: memory_map.len as u64,
        inner: Mutex::new(Inner {
            data: memory_map,
            pos: 0,
            requested_range: SparseRange::default(),
            streamer_state: StreamerState::default(),
            streamer_state_rx: WatchStream::new(state_rx),
            request_tx,
            poll_request_tx: None,
        }),
    };

    let mut contents = [0; 4];
    reader.read_exact(&mut contents).await?;
    assert_eq!(&contents, b"safe");

    streamer.abort();
    let cancellation = streamer
        .await
        .expect_err("the download task must have been cancelled");
    assert!(cancellation.is_cancelled());
    assert!(weak_mapping.upgrade().is_some());

    reader.seek(SeekFrom::Start(0)).await?;
    reader.read_exact(&mut contents).await?;
    assert_eq!(&contents, b"safe");

    drop(reader);
    assert!(weak_mapping.upgrade().is_none());
    Ok(())
}
