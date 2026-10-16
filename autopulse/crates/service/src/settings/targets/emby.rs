use super::{Request, RequestBuilderPerform};
use crate::settings::path_filter::PathFilter;
use crate::settings::rewrite::Rewrite;
use crate::settings::targets::TargetProcess;
use anyhow::Context;
use autopulse_database::models::ScanEvent;
use autopulse_utils::{get_url, RuntimePath};
use reqwest::header;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt::Display, io::Cursor};
use struson::{
    json_path,
    reader::{JsonReader, JsonStreamReader},
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error, info, warn};

#[doc(hidden)]
const fn default_true() -> bool {
    true
}

#[derive(Serialize, Clone, Deserialize)]
pub struct Emby {
    /// URL to the Jellyfin/Emby server
    pub url: String,
    /// API token for the Jellyfin/Emby server
    pub token: String,
    /// Metadata refresh mode (default: `FullRefresh`)
    #[serde(default)]
    pub metadata_refresh_mode: EmbyMetadataRefreshMode,
    /// Whether to try to refresh metadata for the item instead of scan (default: true)
    #[serde(default = "default_true")]
    pub refresh_metadata: bool,
    /// Rewrite path for the file
    pub rewrite: Option<Rewrite>,
    /// Path filter matched against the target-rewritten path.
    #[serde(default)]
    pub filter: PathFilter,
    /// HTTP request options
    #[serde(default)]
    pub request: Request,
    /// How library locations are compared to incoming paths. Default `case_sensitive`.
    #[serde(default)]
    pub path_match: PathMatch,
}

/// How library locations are compared to incoming paths.
#[derive(Serialize, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathMatch {
    /// Exact byte-for-byte prefix match (default; safe on Linux).
    #[default]
    CaseSensitive,
    /// Lowercase both sides before comparing. Useful on Windows / UNC paths.
    CaseInsensitive,
}

/// Metadata refresh mode for Jellyfin/Emby
#[derive(Serialize, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum EmbyMetadataRefreshMode {
    /// `none`
    None,
    /// `validation_only`
    ValidationOnly,
    /// `default`
    Default,
    /// `full_refresh`
    #[default]
    FullRefresh,
}

impl Display for EmbyMetadataRefreshMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match self {
            Self::None => "None",
            Self::ValidationOnly => "ValidationOnly",
            Self::Default => "Default",
            Self::FullRefresh => "FullRefresh",
        };

        write!(f, "{mode}")
    }
}

#[derive(Deserialize, Clone, Eq, PartialEq, Hash)]
#[serde(rename_all = "PascalCase")]
#[doc(hidden)]
struct Library {
    #[allow(dead_code)]
    name: String,
    locations: Vec<String>,
    item_id: String,
    collection_type: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
#[doc(hidden)]
struct UpdateRequest {
    path: String,
    update_type: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
#[doc(hidden)]
struct ScanPayload {
    updates: Vec<UpdateRequest>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
struct ScanPathRequest {
    path: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
struct ScanPathsRequest {
    paths: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase")]
struct ScanPathResponse {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase")]
struct ScanPathsResponse {
    results: Vec<ScanPathResponse>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum MediaServer {
    Jellyfin,
    Emby,
}

impl Display for MediaServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Jellyfin => f.write_str("Jellyfin"),
            Self::Emby => f.write_str("Emby"),
        }
    }
}

enum PluginCall<T> {
    Response(T),
    Unavailable,
    Failed(anyhow::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PluginResult {
    Success,
    Missing,
    Failed,
}

impl ScanPathResponse {
    fn result(&self) -> PluginResult {
        match self.status.as_str() {
            "Created" | "Refreshed" | "Discovered" | "Removed" => PluginResult::Success,
            "PathNotFound" | "ParentNotFound" => PluginResult::Missing,
            _ => PluginResult::Failed,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
#[doc(hidden)]
struct Item {
    id: String,
    path: Option<String>,
}

impl Emby {
    fn get_client(&self) -> anyhow::Result<reqwest::Client> {
        let mut headers = header::HeaderMap::new();

        headers.insert("X-Emby-Token", self.token.parse()?);
        headers.insert(
            "Authorization",
            format!("MediaBrowser Token=\"{}\"", self.token).parse()?,
        );
        headers.insert("Accept", "application/json".parse()?);

        self.request
            .client_builder(headers)
            .build()
            .map_err(Into::into)
    }

    async fn libraries(&self) -> anyhow::Result<Vec<Library>> {
        let client = self.get_client()?;
        let url = get_url(&self.url)?.join("Library/VirtualFolders")?;

        let res = client.get(url).perform().await?;

        Ok(res.json().await?)
    }

    fn get_libraries(&self, libraries: &[Library], path: &str) -> Vec<Library> {
        let mut matched: Vec<Library> = vec![];

        for library in libraries {
            for location in &library.locations {
                if self.path_prefix_matches(location, path) {
                    matched.push(library.clone());
                    break; // one location is enough to match this library
                }
            }
        }

        matched
    }

    fn path_prefix_matches(&self, location: &str, ev_path: &str) -> bool {
        let location = RuntimePath::new(location);
        let event = RuntimePath::new(ev_path);

        match self.path_match {
            PathMatch::CaseSensitive => event.starts_with_case_sensitive(location),
            PathMatch::CaseInsensitive => event.starts_with_ascii_case_insensitive(location),
        }
    }

    async fn _get_item(&self, library: &Library, path: &str) -> anyhow::Result<Option<Item>> {
        let client = self.get_client()?;
        let mut url = get_url(&self.url)?.join("Items")?;

        url.query_pairs_mut().append_pair("Recursive", "true");
        url.query_pairs_mut().append_pair("Fields", "Path");
        url.query_pairs_mut().append_pair("EnableImages", "false");
        if let Some(collection_type) = &library.collection_type {
            url.query_pairs_mut().append_pair(
                "IncludeItemTypes",
                match collection_type.as_str() {
                    "tvshows" => "Episode",
                    "books" => "Book",
                    "music" => "Audio",
                    "movie" => "VideoFile,Movie",
                    _ => "",
                },
            );
        }
        url.query_pairs_mut()
            .append_pair("ParentId", &library.item_id);
        url.query_pairs_mut()
            .append_pair("EnableTotalRecordCount", "false");

        let res = client.get(url).perform().await?;

        // Possibly unneeded unless we can use streams
        let bytes = res.bytes().await?;

        let mut json_reader = JsonStreamReader::new(Cursor::new(bytes));

        json_reader.seek_to(&json_path!["Items"])?;
        json_reader.begin_array()?;

        while json_reader.has_next()? {
            let item: Item = json_reader.deserialize_next()?;

            if item.path == Some(path.to_owned()) {
                return Ok(Some(item));
            }
        }

        Ok(None)
    }

    fn fetch_items(
        &self,
        library: &Library,
    ) -> anyhow::Result<(
        UnboundedReceiver<Item>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    )> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let limit = 1000;

        let client = self.get_client()?;
        let mut url = get_url(&self.url)?.join("Items")?;

        url.query_pairs_mut().append_pair("Recursive", "true");
        url.query_pairs_mut().append_pair("Fields", "Path");
        url.query_pairs_mut().append_pair("EnableImages", "false");
        url.query_pairs_mut()
            .append_pair("ParentId", &library.item_id);
        url.query_pairs_mut()
            .append_pair("EnableTotalRecordCount", "false");
        url.query_pairs_mut()
            .append_pair("Limit", &limit.to_string());
        if let Some(collection_type) = &library.collection_type {
            url.query_pairs_mut().append_pair(
                "IncludeItemTypes",
                match collection_type.as_str() {
                    "tvshows" => "Episode",
                    "books" => "Book",
                    "music" => "Audio",
                    "movie" => "VideoFile,Movie",
                    _ => "",
                },
            );
        }

        let handle = tokio::spawn(async move {
            let mut page = 0;

            loop {
                let mut page_url = url.clone();
                page_url
                    .query_pairs_mut()
                    .append_pair("StartIndex", &(page * limit).to_string());

                let res = client.get(page_url).perform().await?;

                let bytes = res.bytes().await?;

                let mut json_reader = JsonStreamReader::new(Cursor::new(bytes));

                json_reader.seek_to(&json_path!["Items"])?;
                json_reader.begin_array()?;

                let mut found_items_count = 0;

                while json_reader.has_next()? {
                    let item: Item = json_reader.deserialize_next()?;

                    tx.send(item)?;

                    found_items_count += 1;
                }

                if found_items_count < limit {
                    break;
                }

                page += 1;
            }

            drop(tx);

            Ok(())
        });

        Ok((rx, handle))
    }

    async fn get_items<'a>(
        &self,
        library: &Library,
        events: Vec<&'a ScanEvent>,
    ) -> anyhow::Result<(Vec<(&'a ScanEvent, Item)>, Vec<&'a ScanEvent>)> {
        let (mut rx, handle) = self.fetch_items(library)?;

        let mut found_in_library = Vec::new();
        let mut not_found_in_library = events.clone();

        while let Some(item) = rx.recv().await {
            if let Some(ev) = events
                .iter()
                .find(|ev| item.path == Some(ev.get_path(&self.rewrite)))
            {
                found_in_library.push((*ev, item.clone()));
                not_found_in_library.retain(|&e| e.id != ev.id);

                if not_found_in_library.is_empty() {
                    break;
                }
            }
        }

        handle.abort();

        Ok((found_in_library, not_found_in_library))
    }

    // not as effective as refreshing the item, but good enough
    async fn scan(&self, ev: &[&ScanEvent]) -> anyhow::Result<()> {
        let client = self.get_client()?;
        let url = get_url(&self.url)?.join("Library/Media/Updated")?;

        let updates = ev
            .iter()
            .map(|ev| UpdateRequest {
                path: ev.get_path(&self.rewrite),
                update_type: "Modified".to_string(),
            })
            .collect();

        let body = ScanPayload { updates };

        client
            .post(url)
            .header("Content-Type", "application/json")
            .json(&body)
            .perform()
            .await
            .map(|_| ())
    }

    async fn refresh_item(&self, item: &Item) -> anyhow::Result<()> {
        let client = self.get_client()?;
        let mut url = get_url(&self.url)?.join(&format!("Items/{}/Refresh", item.id))?;

        url.query_pairs_mut().append_pair(
            "MetadataRefreshMode",
            &self.metadata_refresh_mode.to_string(),
        );
        url.query_pairs_mut()
            .append_pair("ImageRefreshMode", &self.metadata_refresh_mode.to_string());
        url.query_pairs_mut()
            .append_pair("ReplaceAllMetadata", "true");
        url.query_pairs_mut().append_pair("Recursive", "true");

        // TODO: Possible options in future?
        url.query_pairs_mut()
            .append_pair("ReplaceAllImages", "false");
        url.query_pairs_mut()
            .append_pair("RegenerateTrickplay", "false");

        client.post(url).perform().await.map(|_| ())
    }

    fn endpoint_unavailable(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 404 | 405 | 501)
    }

    async fn targeted_scan_batch(&self, paths: Vec<String>) -> PluginCall<ScanPathsResponse> {
        let result = async {
            let client = self.get_client()?;
            let url = get_url(&self.url)?.join("Library/ScanPaths")?;
            let response = client
                .post(url)
                .header("Content-Type", "application/json")
                .json(&ScanPathsRequest { paths })
                .send()
                .await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            if let Ok(parsed) = serde_json::from_str::<ScanPathsResponse>(&body) {
                return Ok(PluginCall::Response(parsed));
            }
            if Self::endpoint_unavailable(status) {
                return Ok(PluginCall::Unavailable);
            }
            anyhow::bail!("ScanPaths returned {status}: {body}")
        }
        .await;

        match result {
            Ok(call) => call,
            Err(error) => PluginCall::Failed(error),
        }
    }

    async fn targeted_scan(&self, path: String) -> PluginCall<ScanPathResponse> {
        let result = async {
            let client = self.get_client()?;
            let url = get_url(&self.url)?.join("Library/ScanPath")?;
            let response = client
                .post(url)
                .header("Content-Type", "application/json")
                .json(&ScanPathRequest { path })
                .send()
                .await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            // Jellyfin uses HTTP 404 for the plugin's PathNotFound and
            // ParentNotFound results. Parse the contract before classifying the
            // endpoint as absent. Emby normally returns these in a 200 response.
            if let Ok(parsed) = serde_json::from_str::<ScanPathResponse>(&body) {
                return Ok(PluginCall::Response(parsed));
            }
            if Self::endpoint_unavailable(status) {
                return Ok(PluginCall::Unavailable);
            }
            anyhow::bail!("ScanPath returned {status}: {body}")
        }
        .await;

        match result {
            Ok(call) => call,
            Err(error) => PluginCall::Failed(error),
        }
    }

    async fn standard_process(
        &self,
        evs: &[&ScanEvent],
        libraries: &[Library],
    ) -> anyhow::Result<Vec<String>> {
        let library_map = self.get_libraries_for_events(libraries, evs);
        let mut succeeded = HashMap::new();

        let mut to_find = HashMap::new();
        let mut to_refresh = Vec::new();
        let mut to_scan = Vec::new();

        if self.refresh_metadata {
            for ev in evs {
                let ev_path = ev.get_path(&self.rewrite);

                let matched_libraries = library_map.get(&ev.id).cloned().unwrap_or_default();

                if matched_libraries.is_empty() {
                    let known: Vec<&str> = libraries
                        .iter()
                        .flat_map(|l| l.locations.iter().map(String::as_str))
                        .collect();
                    error!(
                        "failed to find library for file '{ev_path}'. Known locations: {known:?}"
                    );
                    continue;
                }

                for library in matched_libraries {
                    to_find.entry(library).or_insert_with(Vec::new).push(*ev);
                }
            }

            for (library, library_events) in to_find {
                let (found_in_library, not_found_in_library) = self
                    .get_items(&library, library_events.clone())
                    .await
                    .with_context(|| {
                        format!(
                            "failed to fetch items for library: {}",
                            library.name.clone()
                        )
                    })?;

                to_refresh.extend(found_in_library);
                to_scan.extend(not_found_in_library);
            }

            for (ev, item) in to_refresh {
                match self.refresh_item(&item).await {
                    Ok(()) => {
                        debug!("refreshed item: {}", item.id);
                        *succeeded.entry(ev.id.clone()).or_insert(true) &= true;
                    }
                    Err(e) => {
                        error!("failed to refresh item: {}", e);
                        succeeded.insert(ev.id.clone(), false);
                    }
                }
            }
        } else {
            to_scan.extend(evs.iter().copied());
        }

        if !to_scan.is_empty() {
            match self.scan(&to_scan).await {
                Ok(()) => {
                    for ev in &to_scan {
                        debug!("scanned file: {}", ev.file_path);

                        *succeeded.entry(ev.id.clone()).or_insert(true) &= true;
                    }
                }
                Err(e) => {
                    error!("failed to scan items: {}", e);

                    for ev in &to_scan {
                        succeeded.insert(ev.id.clone(), false);
                    }
                }
            }
        }

        Ok(succeeded
            .iter()
            .filter_map(|(k, v)| if *v { Some(k.clone()) } else { None })
            .collect())
    }

    fn get_libraries_for_events(
        &self,
        libraries: &[Library],
        evs: &[&ScanEvent],
    ) -> HashMap<String, Vec<Library>> {
        evs.iter()
            .map(|ev| {
                (
                    ev.id.clone(),
                    self.get_libraries(libraries, &ev.get_path(&self.rewrite)),
                )
            })
            .collect()
    }

    fn record_plugin_result(
        succeeded: &mut Vec<String>,
        retry: &mut Vec<String>,
        ev: &ScanEvent,
        path: &str,
        result: &ScanPathResponse,
    ) {
        match result.result() {
            PluginResult::Success => {
                info!(
                    "targeted scan succeeded for {path}: {} ({})",
                    result.item_id, result.status
                );
                succeeded.push(ev.id.clone());
            }
            PluginResult::Missing if ev.found_status == "missing" => {
                debug!("targeted deletion completed for {path}: {}", result.status);
                succeeded.push(ev.id.clone());
            }
            PluginResult::Missing => {
                // Imports can arrive before a remote mount is visible. Let the
                // normal event scheduler retry instead of permanently dropping
                // PathNotFound/ParentNotFound.
                warn!("targeted scan delayed for {path}: {}", result.status);
                retry.push(ev.id.clone());
            }
            PluginResult::Failed => {
                warn!(
                    "targeted scan failed for {path}: {} ({})",
                    result.status, result.message
                );
                retry.push(ev.id.clone());
            }
        }
    }

    pub(super) async fn process_for(
        &self,
        server: MediaServer,
        evs: &[&ScanEvent],
    ) -> anyhow::Result<Vec<String>> {
        let libraries = self
            .libraries()
            .await
            .context("failed to fetch libraries")?;
        let library_matches = self.get_libraries_for_events(&libraries, evs);
        let mut candidates = Vec::new();

        for ev in evs {
            let path = ev.get_path(&self.rewrite);
            if library_matches.get(&ev.id).is_none_or(Vec::is_empty) {
                error!("no {server} library contains targeted path '{path}'");
                continue;
            }
            candidates.push((*ev, path));
        }

        let mut succeeded = Vec::new();
        let mut retry = Vec::new();
        let mut plugin_unavailable = Vec::new();

        let paths = candidates.iter().map(|(_, path)| path.clone()).collect();
        match self.targeted_scan_batch(paths).await {
            PluginCall::Response(batch) => {
                let by_path: HashMap<&str, &ScanPathResponse> = batch
                    .results
                    .iter()
                    .filter_map(|result| {
                        let path = if result.path.is_empty() {
                            // v1.3 briefly put the path in Message; retaining
                            // this read keeps that deployed plugin compatible.
                            result.message.as_str()
                        } else {
                            result.path.as_str()
                        };
                        (!path.is_empty()).then_some((path, result))
                    })
                    .collect();

                for (ev, path) in candidates {
                    if let Some(result) = by_path.get(path.as_str()) {
                        Self::record_plugin_result(&mut succeeded, &mut retry, ev, &path, result);
                    } else {
                        warn!("{server} ScanPaths omitted result for '{path}'");
                        retry.push(ev.id.clone());
                    }
                }
            }
            PluginCall::Failed(error) => {
                warn!("{server} ScanPaths transient failure: {error:#}");
                retry.extend(candidates.iter().map(|(ev, _)| ev.id.clone()));
            }
            PluginCall::Unavailable => {
                info!("{server} ScanPaths is unavailable; trying ScanPath");
                for (ev, path) in candidates {
                    match self.targeted_scan(path.clone()).await {
                        PluginCall::Response(result) => Self::record_plugin_result(
                            &mut succeeded,
                            &mut retry,
                            ev,
                            &path,
                            &result,
                        ),
                        PluginCall::Failed(error) => {
                            warn!("{server} ScanPath transient failure for '{path}': {error:#}");
                            retry.push(ev.id.clone());
                        }
                        PluginCall::Unavailable => plugin_unavailable.push(ev),
                    }
                }
            }
        }

        if !plugin_unavailable.is_empty() {
            warn!(
                "{server} TargetedScan plugin is unavailable for {} path(s)",
                plugin_unavailable.len()
            );
            let safe_fallback: Vec<&ScanEvent> = plugin_unavailable
                .iter()
                .copied()
                .filter(|ev| ev.found_status != "missing")
                .collect();
            retry.extend(
                plugin_unavailable
                    .iter()
                    .filter(|ev| ev.found_status == "missing")
                    .map(|ev| ev.id.clone()),
            );

            // Standard Jellyfin/Emby APIs remain a compatibility fallback for
            // imports. They are unsafe for deletions because item refresh can
            // leave a stale, playable database entry behind.
            succeeded.extend(self.standard_process(&safe_fallback, &libraries).await?);
        }

        succeeded.sort();
        succeeded.dedup();
        debug!("{} targeted scan path(s) will retry", retry.len());
        Ok(succeeded)
    }
}

impl TargetProcess for Emby {
    async fn process(&self, evs: &[&ScanEvent]) -> anyhow::Result<Vec<String>> {
        self.process_for(MediaServer::Emby, evs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopulse_database::models::{FoundStatus, ProcessStatus};
    use chrono::Utc;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    struct MockResponse {
        status: u16,
        body: &'static str,
    }

    fn mock_server(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end;
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = end + 4;
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .or_else(|| line.strip_prefix("Content-Length:"))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                }
                let _ = tx.send(String::from_utf8_lossy(&request).into_owned());

                let reason = match response.status {
                    200 => "OK",
                    204 => "No Content",
                    404 => "Not Found",
                    503 => "Service Unavailable",
                    _ => "Response",
                };
                let payload = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    reason,
                    response.body.len(),
                    response.body
                );
                stream.write_all(payload.as_bytes()).unwrap();
            }
        });

        (format!("http://{address}"), rx)
    }

    const LIBRARIES: &str =
        r#"[{"Name":"TV","Locations":["/media"],"ItemId":"library","CollectionType":"tvshows"}]"#;

    fn response(status: u16, body: &'static str) -> MockResponse {
        MockResponse { status, body }
    }

    fn event(id: &str, path: &str, found_status: FoundStatus) -> ScanEvent {
        let now = Utc::now().naive_utc();
        ScanEvent {
            id: id.to_string(),
            event_source: "sonarr".to_string(),
            event_timestamp: now,
            file_path: path.to_string(),
            file_hash: None,
            process_status: ProcessStatus::Pending.into(),
            found_status: found_status.into(),
            failed_times: 0,
            next_retry_at: None,
            targets_hit: String::new(),
            found_at: None,
            processed_at: None,
            created_at: now,
            updated_at: now,
            can_process: now,
        }
    }

    fn target_at(url: String) -> Emby {
        Emby { url, ..target() }
    }

    fn lib(name: &str, paths: &[&str]) -> Library {
        Library {
            name: name.to_string(),
            locations: paths.iter().map(|s| (*s).to_string()).collect(),
            item_id: format!("id-{name}"),
            collection_type: None,
        }
    }

    fn target() -> Emby {
        Emby {
            url: "http://x".to_string(),
            token: "t".to_string(),
            metadata_refresh_mode: EmbyMetadataRefreshMode::default(),
            refresh_metadata: true,
            rewrite: None,
            filter: PathFilter::default(),
            request: Request::default(),
            path_match: PathMatch::default(),
        }
    }

    #[test]
    fn matches_when_library_location_is_exact_prefix() {
        let libs = vec![lib("TV", &["/media/TV"])];
        let m = target().get_libraries(&libs, "/media/TV/Show/S01E01.mkv");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn matches_when_library_location_has_trailing_slash() {
        let libs = vec![lib("TV", &["/media/TV/"])];
        let m = target().get_libraries(&libs, "/media/TV/Show/S01E01.mkv");
        assert_eq!(
            m.len(),
            1,
            "trailing slash on library location must not break match"
        );
    }

    #[test]
    fn no_match_when_path_outside_library() {
        let libs = vec![lib("TV", &["/data/TV"])];
        let m = target().get_libraries(&libs, "/media/TV/Show.mkv");
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn case_insensitive_matching_when_opted_in() {
        let mut t = target();
        t.path_match = PathMatch::CaseInsensitive;
        let libs = vec![lib("TV", &["/Media/TV"])];
        let m = t.get_libraries(&libs, "/media/tv/Show.mkv");
        assert_eq!(
            m.len(),
            1,
            "case-insensitive must match across case differences"
        );
    }

    #[test]
    fn case_sensitive_matching_by_default() {
        let libs = vec![lib("TV", &["/Media/TV"])];
        let m = target().get_libraries(&libs, "/media/tv/Show.mkv");
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn case_sensitive_mode_parses_windows_paths_but_preserves_case_policy() {
        let libs = vec![lib("TV", &[r"C:\Media\TV\"])];
        assert_eq!(
            target()
                .get_libraries(&libs, r"C:\Media\TV\Show\S01E01.mkv")
                .len(),
            1
        );
        assert_eq!(
            target()
                .get_libraries(&libs, r"c:\media\tv\Show\S01E01.mkv")
                .len(),
            0
        );
    }

    #[test]
    fn case_insensitive_mode_folds_windows_ascii_case() {
        let mut emby = target();
        emby.path_match = PathMatch::CaseInsensitive;
        let libs = vec![lib("TV", &[r"\\SERVER\MEDIA\TV"])];

        assert_eq!(
            emby.get_libraries(&libs, r"\\server\media\tv\Show\S01E01.mkv")
                .len(),
            1
        );
    }

    #[test]
    fn library_prefix_requires_a_component_boundary() {
        let libs = vec![lib("TV", &[r"\\server\media"])];
        assert!(target()
            .get_libraries(&libs, r"\\server\media-archive\Film.mkv")
            .is_empty());
    }

    #[test]
    fn library_prefix_does_not_cross_runtime_flavors() {
        let mut emby = target();
        emby.path_match = PathMatch::CaseInsensitive;
        let libs = vec![lib("TV", &["/media/TV"])];
        assert!(emby
            .get_libraries(&libs, r"C:\media\TV\Show\S01E01.mkv")
            .is_empty());
    }

    #[tokio::test]
    async fn existing_item_refreshes_through_plugin() {
        let (url, requests) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/existing.mkv","ItemId":"item","Status":"Refreshed"}]}"#,
            ),
        ]);
        let ev = event("existing", "/media/existing.mkv", FoundStatus::Found);

        let succeeded = target_at(url)
            .process_for(MediaServer::Jellyfin, &[&ev])
            .await
            .unwrap();

        assert_eq!(succeeded, vec!["existing"]);
        let sent: Vec<_> = requests.iter().collect();
        assert!(sent[1].starts_with("POST /Library/ScanPaths"));
        assert!(!sent.iter().any(|request| request.contains("/Items/")));
    }

    #[tokio::test]
    async fn new_item_is_discovered_through_plugin() {
        let (url, _) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/new.mkv","ItemId":"new","Status":"Created"}]}"#,
            ),
        ]);
        let ev = event("new", "/media/new.mkv", FoundStatus::Found);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Jellyfin, &[&ev])
                .await
                .unwrap(),
            vec!["new"]
        );
    }

    #[tokio::test]
    async fn removed_and_already_missing_deletions_are_successful() {
        let (url, _) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/removed.mkv","ItemId":"old","Status":"Removed"},{"Path":"/media/gone.mkv","Status":"PathNotFound"}]}"#,
            ),
        ]);
        let removed = event("removed", "/media/removed.mkv", FoundStatus::Missing);
        let gone = event("gone", "/media/gone.mkv", FoundStatus::Missing);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Jellyfin, &[&removed, &gone])
                .await
                .unwrap(),
            vec!["gone", "removed"]
        );
    }

    #[tokio::test]
    async fn rename_batch_processes_old_and_new_paths() {
        let (url, _) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/old.mkv","Status":"Removed"},{"Path":"/media/new.mkv","ItemId":"new","Status":"Created"}]}"#,
            ),
        ]);
        let old = event("old", "/media/old.mkv", FoundStatus::Missing);
        let new = event("new", "/media/new.mkv", FoundStatus::Found);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Emby, &[&old, &new])
                .await
                .unwrap(),
            vec!["new", "old"]
        );
    }

    #[tokio::test]
    async fn mixed_batch_only_completes_successful_paths() {
        let (url, _) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/ok.mkv","Status":"Refreshed"},{"Path":"/media/retry.mkv","Status":"Failed","Message":"busy"}]}"#,
            ),
        ]);
        let ok = event("ok", "/media/ok.mkv", FoundStatus::Found);
        let retry = event("retry", "/media/retry.mkv", FoundStatus::Found);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Jellyfin, &[&ok, &retry])
                .await
                .unwrap(),
            vec!["ok"]
        );
    }

    #[tokio::test]
    async fn missing_import_retries_for_delayed_mount_visibility() {
        let (url, _) = mock_server(vec![
            response(200, LIBRARIES),
            response(
                200,
                r#"{"Results":[{"Path":"/media/late.mkv","Status":"ParentNotFound"}]}"#,
            ),
        ]);
        let ev = event("late", "/media/late.mkv", FoundStatus::Found);

        assert!(target_at(url)
            .process_for(MediaServer::Jellyfin, &[&ev])
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn unavailable_plugin_never_refreshes_deleted_item() {
        let (url, requests) = mock_server(vec![
            response(200, LIBRARIES),
            response(404, "not found"),
            response(404, "not found"),
        ]);
        let ev = event("deleted", "/media/deleted.mkv", FoundStatus::Missing);

        assert!(target_at(url)
            .process_for(MediaServer::Jellyfin, &[&ev])
            .await
            .unwrap()
            .is_empty());
        let sent: Vec<_> = requests.iter().collect();
        assert_eq!(sent.len(), 3);
        assert!(!sent.iter().any(|request| request.contains("/Items")));
    }

    #[tokio::test]
    async fn unavailable_batch_endpoint_falls_back_to_scan_path() {
        let (url, requests) = mock_server(vec![
            response(200, LIBRARIES),
            response(404, "not found"),
            response(
                200,
                r#"{"ItemId":"new","Status":"Created","Message":"created"}"#,
            ),
        ]);
        let ev = event("new", "/media/new.mkv", FoundStatus::Found);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Jellyfin, &[&ev])
                .await
                .unwrap(),
            vec!["new"]
        );
        let sent: Vec<_> = requests.iter().collect();
        assert!(sent[2].starts_with("POST /Library/ScanPath"));
    }

    #[tokio::test]
    async fn unavailable_plugin_uses_standard_existing_item_fallback_for_import() {
        let (url, requests) = mock_server(vec![
            response(200, LIBRARIES),
            response(404, "not found"),
            response(404, "not found"),
            response(
                200,
                r#"{"Items":[{"Id":"item","Path":"/media/existing.mkv"}]}"#,
            ),
            response(204, ""),
        ]);
        let ev = event("existing", "/media/existing.mkv", FoundStatus::Found);

        assert_eq!(
            target_at(url)
                .process_for(MediaServer::Jellyfin, &[&ev])
                .await
                .unwrap(),
            vec!["existing"]
        );
        let sent: Vec<_> = requests.iter().collect();
        assert!(sent.iter().any(|request| request.contains("GET /Items?")));
        assert!(sent
            .iter()
            .any(|request| request.contains("POST /Items/item/Refresh")));
    }

    #[tokio::test]
    async fn transient_batch_failure_retries_without_unsafe_fallback() {
        let (url, requests) = mock_server(vec![response(200, LIBRARIES), response(503, "busy")]);
        let ev = event("retry", "/media/retry.mkv", FoundStatus::Missing);

        assert!(target_at(url)
            .process_for(MediaServer::Jellyfin, &[&ev])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(requests.iter().count(), 2);
    }
}
