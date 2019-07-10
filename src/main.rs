
extern crate gotham;
#[macro_use]
extern crate gotham_derive;
extern crate hyper;
extern crate mime;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use std::path::Path;

use gotham::router::builder::*;
use gotham::state::{FromState, State};

use git2::Repository;

use hyper::{Body, Response, StatusCode};

#[derive(Deserialize, StateData, StaticResponseExtender)]
struct PathExtractor {
    commit: String,

    #[serde(rename = "*")]
    parts: String,
}

fn main() {
    gotham::start(
        "localhost:8976", 
        build_simple_router(|route| {
            route.get("/:commit/*")
                .with_path_extractor::<PathExtractor>()
                .to(get_commit_path_contents_response)
        }));
}

fn get_commit_path_contents_response(state: State) -> (State, Response<Body>) {
    let result: Response<Body> = {
        let path_info = PathExtractor::borrow_from(&state);
        let commit = &path_info.commit;
        let path = Path::new(&path_info.parts);
        let repo = Repository::open("/home/samlh/proj/gitblob").expect("Cannot open repo");
        let body = get_commit_path_contents(&repo, commit, path);

        match body {
            Ok(body) => 
                Response::builder()
                    .status(StatusCode::OK)
                    .body(body.into())
                    .unwrap(),
            Err(err) => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(String::from(err.message()).into())
                    .unwrap()
        }
    };
    (state, result)
}

fn get_commit_path_contents<'r>(repo: &'r Repository, ref_name: &'r str, path: &'r Path) -> Result<Vec<u8>, git2::Error>
{
    let reference = repo.resolve_reference_from_short_name(ref_name)?;
    let commit = reference.peel_to_commit()?;
    let tree = commit.tree()?;
    let entry = tree.get_path(path)?;
    let object = entry.to_object(&repo)?;
    let blob = object.as_blob().ok_or_else(|| git2::Error::from_str("Not a blob"))?;
    Ok(Vec::from(blob.content()))
}