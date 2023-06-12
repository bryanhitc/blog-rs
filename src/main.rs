use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, Router},
};

use tower_http::services::ServeDir;

use std::{collections::HashMap, sync::Arc};

use askama::Template;

#[derive(Template)]
#[template(path = "posts.html")]
struct PostTemplate<'a> {
    title: &'a str,
    date: &'a str,
    body: &'a str,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate<'a, 'b> {
    pub title: &'a str,
    pub links: &'b [&'b str],
}

#[derive(Debug, Clone)]
pub struct Post {
    pub id: u64,
    pub title: String,
    pub date: String,
    pub body: String,
}

#[derive(Debug)]
struct AppState {
    posts: Vec<Post>,
    html_cache: HashMap<String, String>,
}

// Our custom Askama filter to replace spaces with dashes in the title
mod filters {

    // now in our templates with can add tis filter e.g. {{ post_title|rmdash }}
    pub fn rmdashes(title: &str) -> askama::Result<String> {
        Ok(title.replace('-', " "))
    }
}

fn get_html_from_posts(query_title: &str, posts: &[Post]) -> Result<String, StatusCode> {
    let post = posts
        .iter()
        .find(|post| post.title == query_title)
        .ok_or(StatusCode::NOT_FOUND)?;
    get_post_html(post)
}

fn get_post_html(post: &Post) -> Result<String, StatusCode> {
    let template = PostTemplate {
        title: &post.title,
        date: &post.date,
        body: &post.body,
    };
    template
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// post router uses two extractors
// Path to extract the query: localhost:4000/post/thispart
// State that holds a Vec<Post> used to render the post that the query matches
async fn post(
    Path(query_title): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(cached_html) = state.html_cache.get(&query_title) {
        // clone into response buffer. maybe can use Cow instead to trade copy for refcount.
        return Html(cached_html.clone()).into_response();
    }

    match get_html_from_posts(&query_title, &state.posts) {
        Ok(html) => Html(html).into_response(),
        Err(err) => (err, err.to_string()).into_response(),
    }
}

// index router (homepage) will return all blog titles in anchor links
async fn index(state: State<Arc<AppState>>) -> impl IntoResponse {
    let plinks = state
        .posts
        .iter()
        .map(|post_template| post_template.title.as_str())
        .collect::<Vec<_>>();
    let template = IndexTemplate {
        title: "bryanhitc blog",
        links: plinks.as_slice(),
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to render template. Error {}", err),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() {
    let post_body = "# Why Apple Vision Pro Is Reasonably Priced

```rs
// and some rust code

fn main() {
    println!(\"askama is awesome\");
}
```
";
    let posts = {
        let mut posts = vec![Post {
            id: 1,
            title: String::from("Why Apple Vision Pro Is Reasonably Priced"),
            date: "2023-06-12".into(),
            body: post_body.into(),
        }];

        for post in &mut posts {
            post.title = post.title.replace(' ', "-");
        }

        posts
    };
    let cache = posts
        .iter()
        .filter_map(|post| {
            get_post_html(post)
                .map(|html| (post.title.clone(), html))
                .ok()
        })
        .collect();
    let app_state = Arc::new(AppState {
        posts,
        html_cache: cache,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/post/:query_title", get(post))
        .with_state(app_state)
        .nest_service("/assets", ServeDir::new("assets"));
    axum::Server::bind(&"0.0.0.0:4000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
