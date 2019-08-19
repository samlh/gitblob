#[macro_use]
extern crate gotham_derive;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
#[macro_use]
extern crate quick_error;
#[macro_use]
extern crate serde_derive;

use std::{
    collections::HashMap,
    env,
    iter::FromIterator,
    path::{Path, PathBuf},
    str,
    sync::Mutex,
    time::{Duration, SystemTime},
};

use base64;
use env_logger;
use git2::{AttrCheckFlags, Blob, Commit, Index, Oid, Repository};
use gotham::{
    self,
    router::builder::*,
    state::{FromState, State},
};
use headers::{
    self, CacheControl, ContentType, Expires, Header, HeaderMapExt, IfModifiedSince, IfNoneMatch,
    LastModified,
};
use http;
use hyper::{
    self,
    header::{HeaderMap, HeaderName, HeaderValue},
    Body, Response, StatusCode,
};
use mime_guess::from_path;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Git2(err: git2::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Http(err: http::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Io(err: std::io::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Other(err: &'static str) {
            description(err)
        }
    }
}

#[derive(Deserialize, StateData, StaticResponseExtender, Debug)]
struct PathExtractor {
    repo_name: String,

    #[serde(rename = "*")]
    parts: Vec<String>,
}

const NUM_THREADS: usize = 1;

lazy_static! {
    static ref REPO_CACHE: Mutex<HashMap::<String, Vec<Mutex<(Repository, Option<Oid>)>>>> =
        { Mutex::new(HashMap::<String, Vec<Mutex<(Repository, Option<Oid>)>>>::new()) };
}

fn main() {
    env_logger::init();
    let port = env::var("PORT").expect("PORT env not found!");
    gotham::start_with_num_threads(
        format!("127.0.0.1:{}", port),
        build_simple_router(|route| {
            route
                .get("/:repo_name/*")
                .with_path_extractor::<PathExtractor>()
                .to(get_commit_path_contents_handler)
        }),
        NUM_THREADS,
    );
}

fn get_commit_path_contents_handler(state: State) -> (State, Response<Body>) {
    let mut error_body = Body::empty();
    let path_info = PathExtractor::borrow_from(&state);
    let repo_name = &path_info.repo_name;
    let separator = path_info.parts.iter().position(|p| *p == ":");
    if let Some(separator) = separator {
        let (commit, path) = path_info.parts.split_at(separator);
        let commit = commit.join("/");
        let path = PathBuf::from_iter(&path[1..]);

        let if_none_match = HeaderMap::borrow_from(&state).get(IfNoneMatch::name());
        let if_modified_since = HeaderMap::borrow_from(&state).typed_get::<IfModifiedSince>();

        debug!("repo_name = {:?}, commit = {:?}, path = {:?}, if_none_match = {:?}, if_modified_since = {:?}", repo_name, &commit, &path, if_none_match, if_modified_since);

        match get_commit_path_contents_response(
            repo_name,
            &commit,
            &path,
            if_none_match,
            if_modified_since,
        ) {
            Ok(response) => return (state, response),
            Err(err) => error_body = err.to_string().into(),
        }
    }
    (
        state,
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(error_body)
            .unwrap(),
    )
}

fn get_commit_path_contents_response(
    repo_name: &str,
    name: &str,
    path: &Path,
    if_none_match: Option<&HeaderValue>,
    if_modified_since: Option<IfModifiedSince>,
) -> Result<Response<Body>, Error> {
    let repo_mutex = REPO_CACHE
        .lock()
        .unwrap()
        .get_mut(repo_name)
        .and_then(|repo_vec| {
            debug!("repo_vec.len = {:?}", repo_vec.len());
            repo_vec.pop()
        });
    let repo_mutex = match repo_mutex {
        Some(repo_mutex) => repo_mutex,
        None => {
            let repo_path =
                Path::new(&env::var("GIT_ROOT").map_err(|_| Error::Other("GIT_ROOT not set!"))?)
                    .join(repo_name);
            (Repository::open(repo_path)?, None).into()
        }
    };

    let response = {
        let mut repo_and_last_index = repo_mutex.lock().unwrap();
        let (ref repo, ref mut last_index) = *repo_and_last_index;
        get_commit_path_contents_response_for_repo(
            repo_name,
            repo,
            last_index,
            name,
            path,
            if_none_match,
            if_modified_since,
        )
    };

    let mut repo_cache = REPO_CACHE.lock().unwrap();
    let repo_vec = repo_cache
        .entry(repo_name.to_string())
        .or_insert_with(|| Vec::with_capacity(NUM_THREADS));
    if repo_vec.len() <= NUM_THREADS {
        repo_vec.push(repo_mutex);
    }

    response
}

fn get_commit_path_contents_response_for_repo(
    repo_name: &str,
    repo: &Repository,
    last_index: &mut Option<Oid>,
    name: &str,
    path: &Path,
    if_none_match: Option<&HeaderValue>,
    if_modified_since: Option<IfModifiedSince>,
) -> Result<Response<Body>, Error> {
    let (commit, stable, last_modified) = get_commit(repo, name)?;
    let tree = commit.tree()?;
    let tree_id = tree.id();
    let entry = tree.get_path(path)?;
    let object = entry.to_object(repo)?;
    let blob = object.as_blob().ok_or(Error::Other("Not a blob"))?;

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

    if !is_modified {
        return Ok(builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())?);
    }

    let mime_type = from_path(path).first_or_text_plain();

    if *last_index != Some(tree_id) {
        debug!("tree_id = {:?} last_index = {:?}", tree_id, last_index);
        repo.index().unwrap().read_tree(&tree).unwrap();
        *last_index = Some(tree_id);
    }

    let filter = repo.get_attr(
        &path,
        "filter",
        AttrCheckFlags::INDEX_ONLY | AttrCheckFlags::NO_SYSTEM,
    )?;
    debug!("mime_type = {:?}, filter = {:?}", mime_type, filter);

    if filter == Some("lfs") {
        if let Some(lfs_path) = get_lfs_cache_path(repo_name, blob) {
            return Ok(builder
                .status(StatusCode::OK)
                .typed_header(ContentType::from(mime_type))
                .header("X-Accel-Redirect", lfs_path)
                .body(Body::empty())?);
        }
    }

    let contents = Vec::from(blob.content());
    Ok(builder
        .status(StatusCode::OK)
        .typed_header(ContentType::from(mime_type))
        .body(contents.into())?)
}

fn get_commit<'r>(
    repo: &'r Repository,
    name: &str,
) -> Result<(Commit<'r>, bool, Option<SystemTime>), Error> {
    let reference = repo.resolve_reference_from_short_name(name);
    match reference {
        Ok(reference) => {
            let reference = reference.resolve()?;
            let reference_name = reference
                .name()
                .ok_or(Error::Other("reference has no name"))?;
            let reference_path = repo.path().join(reference_name);
            let packed_refs_path = repo.path().join("packed-refs");
            let metadata = reference_path
                .metadata()
                .or_else(|_| packed_refs_path.metadata());
            debug!("name = {:?}, reference = {:?}, reference_path = {:?}, packed_refs_path = {:?}, metadata = {:?}", name, reference_name, reference_path, packed_refs_path, &metadata);

            let last_modified = metadata.and_then(|m| m.modified()).ok();
            Ok((reference.peel_to_commit()?, false, last_modified))
        }
        _ => Ok((repo.find_commit(Oid::from_str(name)?)?, true, None)),
    }
}

fn get_lfs_cache_path(repo_name: &str, blob: &Blob) -> Option<String> {
    let mut lines = str::from_utf8(blob.content()).ok()?.lines();
    let version_line = lines.next()?;
    if version_line != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }
    let oid_line = lines.next()?;
    if !oid_line.starts_with("oid sha256:") {
        return None;
    }
    let lfs_oid = &oid_line[11..];
    let lfs_path = format!(
        "/{}/lfs/objects/{}/{}/{}",
        repo_name,
        &lfs_oid[0..2],
        &lfs_oid[2..4],
        &lfs_oid
    );
    debug!("lfs_oid = {:?}, lfs_path = {:?}", lfs_oid, lfs_path);
    Some(lfs_path)
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
