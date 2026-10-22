//! Azure DevOps REST client authenticated with a personal access token.
//!
//! Used instead of the Azure CLI when `AZURE_DEVOPS_EXT_PAT` is set. Unlike
//! `az devops`, the REST API also works with on-premises Azure DevOps Server,
//! whose remotes look like `https://host/[tfs/]{collection}/{project}/_git/{repo}`.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::cli::AzCli;
use crate::types::{CreatePrRequest, GitHostError, PullRequestDetail, UnifiedPrComment};

/// Same variable the Azure DevOps CLI reads, so one setting covers both.
pub const PAT_ENV: &str = "AZURE_DEVOPS_EXT_PAT";
/// Override for older servers (e.g. `4.1` for TFS 2018).
pub const API_VERSION_ENV: &str = "AZURE_DEVOPS_API_VERSION";
/// 5.0 is supported by Azure DevOps Server 2019 and later as well as the cloud.
const DEFAULT_API_VERSION: &str = "5.0";

#[derive(Debug, Clone)]
pub struct AzureRestClient {
    http: reqwest::Client,
    auth_header: String,
    api_version: String,
}

/// A repository resolved through the REST API.
#[derive(Debug, Clone, PartialEq)]
pub struct RestRepo {
    /// Collection (on-prem) or organization (cloud) URL.
    pub collection_url: String,
    pub project_id: String,
    pub repo_id: String,
    pub web_url: String,
}

impl RestRepo {
    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/{}/_apis/git/repositories/{}/{}",
            self.collection_url, self.project_id, self.repo_id, path
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoResponse {
    id: String,
    web_url: String,
    project: ProjectResponse,
}

#[derive(Deserialize)]
struct ProjectResponse {
    id: String,
    name: String,
}

/// Where the repository may live, derived from its remote URL.
#[derive(Debug, PartialEq)]
struct RepoLocation {
    /// URL up to (excluding) `/_git/`.
    base: String,
    repo: String,
}

impl AzureRestClient {
    pub fn from_env() -> Option<Self> {
        let pat = std::env::var(PAT_ENV).ok().filter(|v| !v.is_empty())?;
        let api_version = std::env::var(API_VERSION_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());
        Some(Self::new(&pat, api_version))
    }

    fn new(pat: &str, api_version: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            auth_header: format!("Basic {}", BASE64.encode(format!(":{pat}"))),
            api_version,
        }
    }

    /// Resolves the repository behind an HTTPS remote or PR URL.
    ///
    /// The path before `/_git/` is either `{collection}/{project}` or, for a
    /// project's default repository, just `{collection}` with the project named
    /// after the repository. Both layouts are tried.
    pub async fn resolve_repo(&self, url: &str) -> Result<RestRepo, GitHostError> {
        let location = parse_repo_location(url)?;

        let candidates = [
            location.base.clone(),
            format!("{}/{}", location.base, location.repo),
        ];
        let mut last_error = None;
        for project_base in candidates {
            let api = format!("{project_base}/_apis/git/repositories/{}", location.repo);
            match self.request(Method::GET, &api, None).await {
                Ok(body) => {
                    let repo: RepoResponse = parse_json(&body)?;
                    return Ok(RestRepo {
                        collection_url: collection_url(&project_base, &repo.project.name),
                        project_id: repo.project.id,
                        repo_id: repo.id,
                        web_url: repo.web_url,
                    });
                }
                Err(GitHostError::RepoNotFoundOrNoAccess(msg)) => {
                    last_error = Some(GitHostError::RepoNotFoundOrNoAccess(msg));
                }
                Err(other) => return Err(other),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            GitHostError::RepoNotFoundOrNoAccess(format!("No Azure DevOps repository at {url}"))
        }))
    }

    pub async fn create_pr(
        &self,
        repo: &RestRepo,
        request: &CreatePrRequest,
    ) -> Result<PullRequestDetail, GitHostError> {
        let mut body = json!({
            "sourceRefName": format!("refs/heads/{}", request.head_branch),
            "targetRefName": format!("refs/heads/{}", request.base_branch),
            "title": request.title,
            "description": request.body.as_deref().unwrap_or(""),
        });
        if request.draft.unwrap_or(false) {
            body["isDraft"] = json!(true);
        }
        let raw = self
            .request(Method::POST, &repo.api_url("pullrequests"), Some(body))
            .await?;
        let pr = AzCli::parse_pr_response(&raw).map_err(GitHostError::from)?;
        Ok(with_web_url(pr, repo))
    }

    pub async fn get_pr(
        &self,
        repo: &RestRepo,
        pr_number: i64,
    ) -> Result<PullRequestDetail, GitHostError> {
        let raw = self
            .request(
                Method::GET,
                &repo.api_url(&format!("pullrequests/{pr_number}")),
                None,
            )
            .await?;
        let pr = AzCli::parse_pr_response(&raw).map_err(GitHostError::from)?;
        Ok(with_web_url(pr, repo))
    }

    pub async fn list_prs(
        &self,
        repo: &RestRepo,
        source_branch: Option<&str>,
        status: &str,
    ) -> Result<Vec<PullRequestDetail>, GitHostError> {
        let mut url = Url::parse(&repo.api_url("pullrequests"))
            .map_err(|e| GitHostError::Repository(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("searchCriteria.status", status);
        if let Some(branch) = source_branch {
            url.query_pairs_mut().append_pair(
                "searchCriteria.sourceRefName",
                &format!("refs/heads/{branch}"),
            );
        }
        let raw = self.request(Method::GET, url.as_str(), None).await?;
        let prs = AzCli::parse_pr_value_list(&raw).map_err(GitHostError::from)?;
        Ok(prs.into_iter().map(|pr| with_web_url(pr, repo)).collect())
    }

    pub async fn get_pr_threads(
        &self,
        repo: &RestRepo,
        pr_number: i64,
    ) -> Result<Vec<UnifiedPrComment>, GitHostError> {
        let raw = self
            .request(
                Method::GET,
                &repo.api_url(&format!("pullRequests/{pr_number}/threads")),
                None,
            )
            .await?;
        AzCli::parse_pr_threads(&raw).map_err(GitHostError::from)
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String, GitHostError> {
        let mut url = Url::parse(url).map_err(|e| GitHostError::Repository(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);

        let mut builder = self
            .http
            .request(method.clone(), url.clone())
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            builder = builder.json(&body);
        }

        tracing::debug!("Azure DevOps REST {} {}", method, url);
        let response = builder.send().await.map_err(|e| {
            GitHostError::PullRequest(format!("Azure DevOps request to {url} failed: {e}"))
        })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        // Azure DevOps answers an invalid PAT with a 203 sign-in page.
        let is_sign_in_page = status == StatusCode::NON_AUTHORITATIVE_INFORMATION
            || (status.is_success() && text.trim_start().starts_with('<'));
        if status.is_success() && !is_sign_in_page {
            return Ok(text);
        }

        let detail = format!("{status} from {url}: {}", truncate(&text, 300));
        Err(match status {
            _ if is_sign_in_page => GitHostError::AuthFailed(format!(
                "Azure DevOps rejected the personal access token ({PAT_ENV}): {detail}"
            )),
            StatusCode::UNAUTHORIZED => GitHostError::AuthFailed(detail),
            StatusCode::FORBIDDEN => GitHostError::InsufficientPermissions(detail),
            StatusCode::NOT_FOUND => GitHostError::RepoNotFoundOrNoAccess(detail),
            _ => GitHostError::PullRequest(detail),
        })
    }
}

/// Extracts the PR number from `.../_git/{repo}/pullrequest/{id}`.
pub fn pr_number_from_url(url: &str) -> Option<i64> {
    let path = Url::parse(url).ok()?.path().to_string();
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    segments.find(|s| s.eq_ignore_ascii_case("pullrequest"))?;
    segments.next()?.parse().ok()
}

fn parse_repo_location(url: &str) -> Result<RepoLocation, GitHostError> {
    let invalid = || {
        GitHostError::Repository(format!(
            "Not an Azure DevOps HTTPS repository URL: {url}. \
             Personal access tokens require an HTTPS remote."
        ))
    };
    let parsed = Url::parse(url).map_err(|_| invalid())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(invalid());
    }
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();
    let git_idx = segments
        .iter()
        .position(|s| *s == "_git")
        .ok_or_else(invalid)?;
    let repo = segments
        .get(git_idx + 1)
        .map(|r| r.strip_suffix(".git").unwrap_or(r).to_string())
        .ok_or_else(invalid)?;

    let mut base = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().ok_or_else(invalid)?
    );
    if let Some(port) = parsed.port() {
        base.push_str(&format!(":{port}"));
    }
    for segment in &segments[..git_idx] {
        base.push('/');
        base.push_str(segment);
    }

    Ok(RepoLocation { base, repo })
}

/// Strips the project segment from a project-scoped base URL.
fn collection_url(project_base: &str, project_name: &str) -> String {
    let (parent, last) = project_base.rsplit_once('/').unwrap_or((project_base, ""));
    let decoded = percent_decode(last);
    if decoded.eq_ignore_ascii_case(project_name) {
        parent.to_string()
    } else {
        project_base.to_string()
    }
}

fn percent_decode(segment: &str) -> String {
    url::form_urlencoded::parse(format!("x={segment}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| segment.to_string())
}

fn with_web_url(mut pr: PullRequestDetail, repo: &RestRepo) -> PullRequestDetail {
    pr.url = format!("{}/pullrequest/{}", repo.web_url, pr.number);
    pr
}

fn parse_json<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, GitHostError> {
    serde_json::from_str(raw).map_err(|e| {
        GitHostError::UnexpectedOutput(format!(
            "Failed to parse Azure DevOps response: {e}; raw: {}",
            truncate(raw, 300)
        ))
    })
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_on_prem_url_with_tfs_prefix() {
        let loc =
            parse_repo_location("https://tfs.corp.local/tfs/DefaultCollection/Payments/_git/api")
                .unwrap();
        assert_eq!(
            loc.base,
            "https://tfs.corp.local/tfs/DefaultCollection/Payments"
        );
        assert_eq!(loc.repo, "api");
    }

    #[test]
    fn parses_on_prem_url_with_port_and_credentials() {
        let loc = parse_repo_location(
            "https://user@devops.corp.local:8443/Coll/My%20Project/_git/web.git",
        )
        .unwrap();
        assert_eq!(loc.base, "https://devops.corp.local:8443/Coll/My%20Project");
        assert_eq!(loc.repo, "web");
    }

    #[test]
    fn parses_cloud_url() {
        let loc = parse_repo_location("https://org@dev.azure.com/org/Proj/_git/repo").unwrap();
        assert_eq!(loc.base, "https://dev.azure.com/org/Proj");
        assert_eq!(loc.repo, "repo");
    }

    #[test]
    fn parses_pr_url() {
        let loc = parse_repo_location(
            "https://tfs.corp.local/DefaultCollection/Proj/_git/repo/pullrequest/42",
        )
        .unwrap();
        assert_eq!(loc.base, "https://tfs.corp.local/DefaultCollection/Proj");
        assert_eq!(loc.repo, "repo");
    }

    #[test]
    fn rejects_ssh_and_non_azure_urls() {
        assert!(parse_repo_location("ssh://tfs.corp.local:22/Coll/Proj/_git/repo").is_err());
        assert!(parse_repo_location("https://github.com/owner/repo").is_err());
    }

    #[test]
    fn collection_url_strips_matching_project_segment() {
        assert_eq!(
            collection_url("https://tfs/tfs/Coll/My%20Project", "My Project"),
            "https://tfs/tfs/Coll"
        );
        // Default-repo layout: the candidate already ends in the repo-named project.
        assert_eq!(
            collection_url("https://tfs/tfs/Coll/api", "api"),
            "https://tfs/tfs/Coll"
        );
        assert_eq!(
            collection_url("https://dev.azure.com/org/Proj", "Proj"),
            "https://dev.azure.com/org"
        );
    }

    #[test]
    fn extracts_pr_number() {
        assert_eq!(
            pr_number_from_url("https://tfs.corp.local/Coll/Proj/_git/repo/pullrequest/42"),
            Some(42)
        );
        assert_eq!(
            pr_number_from_url("https://tfs.corp.local/Coll/Proj/_git/repo"),
            None
        );
    }

    #[test]
    fn auth_header_uses_empty_username() {
        let client = AzureRestClient::new("secret", DEFAULT_API_VERSION.into());
        assert_eq!(
            client.auth_header,
            format!("Basic {}", BASE64.encode(":secret"))
        );
    }

    mod mock_server {
        use std::sync::{Arc, Mutex};

        use axum::{
            Json, Router,
            extract::{Path, Query, State},
            http::{HeaderMap, StatusCode},
            response::{Html, IntoResponse},
            routing::get,
        };
        use serde_json::{Value, json};

        use super::*;

        const PAT: &str = "test-pat";

        #[derive(Clone, Default)]
        struct Recorded {
            created: Arc<Mutex<Option<Value>>>,
            queries: Arc<Mutex<Vec<String>>>,
        }

        fn authorized(headers: &HeaderMap) -> bool {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Basic {}", BASE64.encode(format!(":{PAT}"))))
        }

        fn repo_json(base: &str, project: &str, repo: &str) -> Value {
            json!({
                "id": format!("{repo}-id"),
                "name": repo,
                "webUrl": format!("{base}/_git/{repo}"),
                "project": { "id": format!("{project}-pid"), "name": project },
            })
        }

        fn pr_json(id: i64, status: &str) -> Value {
            json!({
                "pullRequestId": id,
                "status": status,
                "title": "Add feature",
                "sourceRefName": "refs/heads/feature",
                "targetRefName": "refs/heads/main",
            })
        }

        /// Mimics an on-prem server at `/tfs/Coll` with project `Payments`
        /// (repo `api`) and a project `solo` whose default repo is `solo`.
        async fn start() -> (String, Recorded) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let recorded = Recorded::default();

            let o = origin.clone();
            let app = Router::new()
                .route(
                    "/tfs/Coll/{project}/_apis/git/repositories/{repo}",
                    get(move |Path((project, repo)): Path<(String, String)>, headers: HeaderMap| {
                        let o = o.clone();
                        async move {
                            if !authorized(&headers) {
                                // Azure DevOps serves a sign-in page for a bad PAT.
                                return (StatusCode::NON_AUTHORITATIVE_INFORMATION, Html("<html>Sign in</html>")).into_response();
                            }
                            match (project.as_str(), repo.as_str()) {
                                ("Payments", "api") | ("solo", "solo") => {
                                    let base = format!("{o}/tfs/Coll/{project}");
                                    Json(repo_json(&base, &project, &repo)).into_response()
                                }
                                _ => StatusCode::NOT_FOUND.into_response(),
                            }
                        }
                    }),
                )
                .route(
                    "/tfs/Coll/_apis/git/repositories/{repo}",
                    get(|| async { StatusCode::NOT_FOUND }),
                )
                .route(
                    "/tfs/Coll/Payments-pid/_apis/git/repositories/api-id/pullrequests",
                    get(|State(rec): State<Recorded>, Query(q): Query<Vec<(String, String)>>| async move {
                        rec.queries.lock().unwrap().push(
                            q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
                        );
                        Json(json!({ "value": [pr_json(7, "active")], "count": 1 }))
                    })
                    .post(|State(rec): State<Recorded>, Json(body): Json<Value>| async move {
                        *rec.created.lock().unwrap() = Some(body);
                        (StatusCode::CREATED, Json(pr_json(8, "active")))
                    }),
                )
                .route(
                    "/tfs/Coll/Payments-pid/_apis/git/repositories/api-id/pullrequests/{id}",
                    get(|Path(id): Path<i64>| async move { Json(pr_json(id, "completed")) }),
                )
                .route(
                    "/tfs/Coll/Payments-pid/_apis/git/repositories/api-id/pullRequests/{id}/threads",
                    get(|| async {
                        Json(json!({ "value": [{
                            "comments": [
                                { "id": 1, "author": { "displayName": "Ayse" }, "content": "Looks good",
                                  "publishedDate": "2026-10-20T10:00:00Z", "commentType": "text" },
                                { "id": 2, "content": "Policy check", "commentType": "system" }
                            ]
                        }]}))
                    }),
                )
                .with_state(recorded.clone());

            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (origin, recorded)
        }

        fn client(pat: &str) -> AzureRestClient {
            AzureRestClient::new(pat, DEFAULT_API_VERSION.into())
        }

        #[tokio::test]
        async fn resolves_project_scoped_on_prem_repo() {
            let (origin, _) = start().await;
            let repo = client(PAT)
                .resolve_repo(&format!("{origin}/tfs/Coll/Payments/_git/api"))
                .await
                .unwrap();
            assert_eq!(repo.collection_url, format!("{origin}/tfs/Coll"));
            assert_eq!(repo.project_id, "Payments-pid");
            assert_eq!(repo.repo_id, "api-id");
        }

        #[tokio::test]
        async fn resolves_default_repo_url_without_project_segment() {
            let (origin, _) = start().await;
            let repo = client(PAT)
                .resolve_repo(&format!("{origin}/tfs/Coll/_git/solo"))
                .await
                .unwrap();
            assert_eq!(repo.collection_url, format!("{origin}/tfs/Coll"));
            assert_eq!(repo.project_id, "solo-pid");
        }

        #[tokio::test]
        async fn bad_pat_is_reported_as_auth_failure() {
            let (origin, _) = start().await;
            let err = client("wrong")
                .resolve_repo(&format!("{origin}/tfs/Coll/Payments/_git/api"))
                .await
                .unwrap_err();
            assert!(matches!(err, GitHostError::AuthFailed(_)), "{err:?}");
            assert!(!err.should_retry());
        }

        #[tokio::test]
        async fn unknown_repo_is_not_found() {
            let (origin, _) = start().await;
            let err = client(PAT)
                .resolve_repo(&format!("{origin}/tfs/Coll/Payments/_git/missing"))
                .await
                .unwrap_err();
            assert!(
                matches!(err, GitHostError::RepoNotFoundOrNoAccess(_)),
                "{err:?}"
            );
        }

        #[tokio::test]
        async fn pull_request_round_trip() {
            let (origin, recorded) = start().await;
            let rest = client(PAT);
            let repo = rest
                .resolve_repo(&format!("{origin}/tfs/Coll/Payments/_git/api"))
                .await
                .unwrap();
            let web = format!("{origin}/tfs/Coll/Payments/_git/api");

            let created = rest
                .create_pr(
                    &repo,
                    &CreatePrRequest {
                        title: "Add feature".into(),
                        body: Some("Details".into()),
                        head_branch: "feature".into(),
                        base_branch: "main".into(),
                        draft: Some(true),
                        head_repo_url: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(created.number, 8);
            assert_eq!(created.url, format!("{web}/pullrequest/8"));
            let body = recorded.created.lock().unwrap().clone().unwrap();
            assert_eq!(body["sourceRefName"], "refs/heads/feature");
            assert_eq!(body["targetRefName"], "refs/heads/main");
            assert_eq!(body["isDraft"], true);

            let listed = rest.list_prs(&repo, Some("feature"), "all").await.unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].head_branch, "feature");
            let query = recorded.queries.lock().unwrap()[0].clone();
            assert!(
                query.contains("searchCriteria.sourceRefName=refs/heads/feature"),
                "{query}"
            );
            assert!(query.contains("searchCriteria.status=all"), "{query}");
            assert!(query.contains("api-version=5.0"), "{query}");

            let pr_url = format!("{web}/pullrequest/8");
            let fetched = rest
                .get_pr(&rest.resolve_repo(&pr_url).await.unwrap(), 8)
                .await
                .unwrap();
            assert!(matches!(
                fetched.status,
                db::models::merge::MergeStatus::Merged
            ));

            let comments = rest.get_pr_threads(&repo, 8).await.unwrap();
            assert_eq!(comments.len(), 1, "system comments are skipped");
        }
    }
}
