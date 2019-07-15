extern crate base64;
extern crate env_logger;
extern crate gotham;
#[macro_use]
extern crate gotham_derive;
extern crate headers;
extern crate hyper;
extern crate log;
extern crate mime;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use std::env;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use git2::{Commit, Oid, Repository};
use gotham::router::builder::*;
use gotham::state::{FromState, State};
use headers::{
    CacheControl, ContentType, Expires, Header, HeaderMapExt, IfModifiedSince, IfNoneMatch,
    LastModified,
};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Body, Response, StatusCode};
use mime_guess::guess_mime_type_opt;

#[derive(Deserialize, StateData, StaticResponseExtender, Debug)]
struct PathExtractor {
    repo_name: String,

    #[serde(rename = "*")]
    parts: Vec<String>,
}

fn main() {
    env_logger::init();
    let port = env::var("PORT").expect("PORT env not found!");
    gotham::start(
        format!("127.0.0.1:{}", port),
        build_simple_router(|route| {
            route
                .get("/:repo_name/*")
                .with_path_extractor::<PathExtractor>()
                .to(get_commit_path_contents_handler)
        }),
    );
}

fn get_commit_path_contents_handler(state: State) -> (State, Response<Body>) {
    let result: Response<Body> = {
        let path_info = PathExtractor::borrow_from(&state);
        let repo_name = &path_info.repo_name;
        let separator = path_info
            .parts
            .iter()
            .position(|p| *p == ":");
        match separator {
            Some(separator) => {
                let (commit, path) = path_info.parts.split_at(separator);
                let commit = commit.join("/");
                let path = PathBuf::from_iter(&path[1..]);

                let if_none_match = HeaderMap::borrow_from(&state).get(IfNoneMatch::name());
                let if_modified_since = HeaderMap::borrow_from(&state).typed_get::<IfModifiedSince>();

                match get_commit_path_contents_response(
                    repo_name,
                    &commit,
                    &path,
                    if_none_match,
                    if_modified_since,
                ) {
                    Ok(response) => response,
                    Err(err) => {
                        Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(String::from(err.message()).into())
                            .unwrap()
                    }
                }
            },
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap()
        }
    };
    (state, result)
}

fn get_commit_path_contents_response(
    repo_name: &str,
    name: &str,
    path: &Path,
    if_none_match: Option<&HeaderValue>,
    if_modified_since: Option<IfModifiedSince>,
) -> Result<Response<Body>, git2::Error> {
    let repo = Repository::open(
        Path::new(&env::var("GIT_ROOT").expect("GIT_ROOT env not found!")).join(repo_name),
    )?;
    let (commit, stable, last_modified) = get_commit(&repo, name)?;
    let tree = commit.tree()?;
    let entry = tree.get_path(path)?;
    let object = entry.to_object(&repo)?;
    let blob = object
        .as_blob()
        .ok_or_else(|| git2::Error::from_str("Not a blob"))?;

    let max_age = Duration::from_secs(if stable { 60 * 60 * 24 * 30 } else { 60 * 10 });

    let etag = format!("\"{}\"", base64::encode(blob.id().as_bytes()));

    let is_modified = match (last_modified, if_modified_since, if_none_match) {
        (_, _, Some(if_none_match)) => if_none_match
            .to_str()
            .map(|m| !m.contains(&etag))
            .unwrap_or(true),
        (Some(last_modified), Some(if_modified_since), _) => {
            if_modified_since.is_modified(last_modified)
        }
        _ => true,
    };

    let mut builder = Response::builder();
    builder
        .header(headers::ETag::name(), etag)
        .typed_header(CacheControl::new().with_max_age(max_age).with_public())
        .typed_header(Expires::from(SystemTime::now() + max_age));

    if let Some(last_modified) = last_modified {
        builder.typed_header(LastModified::from(last_modified));
    }

    let response = if is_modified {
        let mime_type = guess_mime_type_opt(path).unwrap_or(mime::TEXT_PLAIN_UTF_8);

        builder
            .status(StatusCode::OK)
            .typed_header(ContentType::from(mime_type))
            .body(Vec::from(blob.content()).into())
            .unwrap()
    } else {
        builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap()
    };

    Ok(response)
}

fn get_commit<'r>(
    repo: &'r Repository,
    name: &str,
) -> Result<(Commit<'r>, bool, Option<SystemTime>), git2::Error> {
    let reference = repo.resolve_reference_from_short_name(name);
    match reference {
        Ok(reference) => {
            let reference = reference.resolve()?;
            let reference_path = repo.path().join(reference.name().unwrap());
            let metadata = reference_path.metadata();
            let last_modified = metadata.and_then(|m| m.modified()).ok();
            Ok((reference.peel_to_commit()?, false, last_modified))
        }
        _ => Ok((repo.find_commit(Oid::from_str(name)?)?, true, None)),
    }
}

struct HeadersExtender<'a, 'b> {
    builder: &'a mut hyper::http::response::Builder,
    name: &'b HeaderName,
}

impl<'a, 'b> Extend<HeaderValue> for HeadersExtender<'a, 'b> {
    fn extend<I: IntoIterator<Item = HeaderValue>>(&mut self, iter: I) {
        for v in iter.into_iter() {
            self.builder.header(self.name, v);
        }
    }
}

pub trait ResponseBuilderExt {
    fn typed_header<H: Header>(&mut self, header: H) -> &mut hyper::http::response::Builder;
}

impl ResponseBuilderExt for hyper::http::response::Builder {
    fn typed_header<H: Header>(&mut self, header: H) -> &mut hyper::http::response::Builder {
        let mut extender = HeadersExtender {
            builder: self,
            name: H::name(),
        };
        header.encode(&mut extender);
        self
    }
}
