#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use] extern crate lazy_static;
extern crate rayon;
#[macro_use] extern crate rocket;
#[macro_use] extern crate rocket_contrib;
#[macro_use] extern crate serde_derive;
extern crate serde_json;
extern crate syntect;
extern crate rocket_cors;
extern crate indexmap;

use rocket_contrib::json::{Json, JsonValue};
use rocket_cors::{AllowedOrigins};
use std::env;
use std::path::Path;
use syntect::highlighting::ThemeSet;
use std::panic;
use std::fmt::Write;
use syntect::easy::{HighlightLines, ScopeRegionIterator};
use syntect::highlighting::{Color, Theme};
use syntect::html::{styles_to_coloured_html, IncludeBackground};
use syntect::parsing::{ScopeStack, ParseState, SyntaxDefinition, SyntaxSet};
use indexmap::set::IndexSet;

thread_local! {
    static SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
}

lazy_static! {
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
}

#[derive(Deserialize)]
struct Query {
    // Deprecated field with a default empty string value, kept for backwards
    // compatability with old clients.
    #[serde(default)]
    extension: String,

    // default empty string value for backwards compat with clients who do not specify this field.
    #[serde(default)]
    filepath: String,

    // default false value as this field is optional.
    #[serde(default)]
    scopify: bool,

    theme: String,
    code: String,
}

#[post("/", format = "application/json", data = "<q>")]
fn index(q: Json<Query>) -> JsonValue {
    // TODO(slimsag): In an ideal world we wouldn't be relying on catch_unwind
    // and instead Syntect would return Result types when failures occur. This
    // will require some non-trivial work upstream:
    // https://github.com/trishume/syntect/issues/98
    let result = panic::catch_unwind(|| {
        highlight(q)
    });
    match result {
        Ok(v) => v,
        Err(_) => json!({"error": "panic while highlighting code", "code": "panic"}),
    }
}

fn highlight(q: Json<Query>) -> JsonValue {
    SYNTAX_SET.with(|syntax_set| {
        // Determine syntax definition by extension.
        let mut is_plaintext = false;
        let syntax_def = if q.filepath == "" {
            // Legacy codepath, kept for backwards-compatability with old clients.
            match syntax_set.find_syntax_by_extension(&q.extension) {
                Some(v) => v,
                None =>
                    // Fall back: Determine syntax definition by first line.
                    match syntax_set.find_syntax_by_first_line(&q.code) {
                        Some(v) => v,
                        None => return json!({"error": "invalid extension"}),
                },
            }
        } else {
            // Split the input path ("foo/myfile.go") into file name
            // ("myfile.go") and extension ("go").
            let path = Path::new(&q.filepath);
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let extension = path.extension().and_then(|x| x.to_str()).unwrap_or("");

            // To determine the syntax definition, we must first check using the
            // filename as some syntaxes match an "extension" that is actually a
            // whole file name (e.g. "Dockerfile" or "CMakeLists.txt"); see e.g. https://github.com/trishume/syntect/pull/170
            //
            // After that, if we do not find any syntax, we can actually check by
            // extension and lastly via the first line of the code.

            // First try to find a syntax whose "extension" matches our file
            // name. This is done due to some syntaxes matching an "extension"
            // that is actually a whole file name (e.g. "Dockerfile" or "CMakeLists.txt")
            // see https://github.com/trishume/syntect/pull/170
            match syntax_set.find_syntax_by_extension(file_name) {
                Some(v) => v,
                None => 
                    // Now try to find the syntax by the actual file extension.
                    match syntax_set.find_syntax_by_extension(extension) {
                        Some(v) => v,
                        None =>
                            // Fall back: Determine syntax definition by first line.
                            match syntax_set.find_syntax_by_first_line(&q.code) {
                                Some(v) => v,
                                None => {
                                    is_plaintext = true;

                                    // Render plain text, so the user gets the same HTML
                                    // output structure.
                                    syntax_set.find_syntax_plain_text()
                                }
                        },
                    }
            }
        };

        if q.scopify {
            let (scope_names, regions) = scopify_string_newlines(&q.code, &syntax_def);
            return json!({
                "plaintext": is_plaintext,
                "detected_language": syntax_def.name,
                "scopified_scope_names": scope_names,
                "scopified_regions": regions,
            })
        }


        // Determine theme to use.
        //
        // TODO(slimsag): We could let the query specify the theme file's actual
        // bytes? e.g. via `load_from_reader`.
        let theme = match THEME_SET.themes.get(&q.theme) {
            Some(v) => v,
            None => return json!({"error": "invalid theme", "code": "invalid_theme"}),
        };

        // TODO(slimsag): return the theme's background color (and other info??) to caller?
        // https://github.com/trishume/syntect/blob/c8b47758a3872d478c7fc740782cd468b2c0a96b/examples/synhtml.rs#L24

        json!({
            "data": highlighted_snippet_for_string_newlines(&q.code, &syntax_def, theme),
            "plaintext": is_plaintext,
            "detected_language": syntax_def.name,
        })
    })
}

#[get("/health")]
fn health() -> &'static str {
    "OK"
}

#[catch(404)]
fn not_found() -> JsonValue {
    json!({"error": "resource not found", "code": "resource_not_found"})
}

fn list_features() {
    // List embedded themes.
    println!("## Embedded themes:");
    println!("");
    for t in THEME_SET.themes.keys() {
        println!("- `{}`", t);
    }
    println!("");

    // List supported file extensions.
    SYNTAX_SET.with(|syntax_set| {
        println!("## Supported file extensions:");
        println!("");
        for sd in syntax_set.syntaxes() {
            println!("- {} (`{}`)", sd.name, sd.file_extensions.join("`, `"));
        }
        println!("");
    });
}

/// The same as `syntect::html::highlighted_snippet_for_string` except it is
/// for syntaxes compiled for the newline character mode (`SyntaxSet::load_defaults_newlines()`).
pub fn highlighted_snippet_for_string_newlines(
    s: &str,
    syntax: &SyntaxDefinition,
    theme: &Theme,
) -> String {
    let mut output = String::new();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let c = theme.settings.background.unwrap_or(Color::WHITE);
    write!(
        output,
        "<pre style=\"background-color:#{:02x}{:02x}{:02x};\">\n",
        c.r, c.g, c.b
    ).unwrap();
    for line in LinesWithEndings::from(s) {
        let regions = highlighter.highlight(&line);
        let html = styles_to_coloured_html(&regions[..], IncludeBackground::IfDifferent(c));
        output.push_str(&html);
    }
    output.push_str("</pre>");
    output
}

pub fn scopify_string_newlines(
    s: &str,
    syntax: &SyntaxDefinition,
) -> (Vec<String>, Vec<JsonValue>) {
        let mut state = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut scope_names: IndexSet<String> = IndexSet::new();

        let mut output = Vec::new();
        let mut offset = 0;
        for line in LinesWithEndings::from(s) {
            let ops = state.parse_line(&line);

            for (s, op) in ScopeRegionIterator::new(&ops, &line) {
                stack.apply(op);
                if s.is_empty() { // we don't care about ops applied inbetween bytes
                    continue;
                }

                let mut affected_by_scopes = Vec::new();
                for name in stack.as_slice() {
                   let (name_index, _) = scope_names.insert_full(name.to_string());
                   affected_by_scopes.push(name_index);
                }
                output.push(json!({"offset": offset, "length": s.len(), "scopes": affected_by_scopes}));
                offset += s.len();
            }
        }

        let mut scope_names_serializable = Vec::new();
        for scope_name in scope_names.iter() {
            scope_names_serializable.push(scope_name.clone())
        }
        (scope_names_serializable, output)
}

fn main() {
    // Only list features if QUIET != "true"
    match env::var("QUIET") {
        Ok(v) => if v != "true" {
            list_features()
        },
        Err(_) => list_features(),
    };

    let mut r = rocket::ignite().mount("/", routes![index, health]);

    // CORS handling
    let cors: Option<rocket_cors::Cors> = match env::var("SYNTECT_SERVER_ALLOW_ORIGIN_STAR") {
        Ok(v) => if v == "true" {
            let cors = rocket_cors::CorsOptions {
                allowed_origins: AllowedOrigins::all(),
                ..Default::default()
            };
            Some(cors.to_cors().unwrap())
        } else { None },
        Err(_) => None,
    };
    match cors {
        Some(v) => r = {r.attach(v)} ,
        None => {}
    };

    r.register(catchers![not_found])
        .launch();
}


/// Iterator yielding every line in a string. The line includes newline character(s).
/// 
/// Borrowed from https://stackoverflow.com/a/40457615
pub struct LinesWithEndings<'a> {
    input: &'a str,
}

impl<'a> LinesWithEndings<'a> {
    pub fn from(input: &'a str) -> LinesWithEndings<'a> {
        LinesWithEndings {
            input: input,
        }
    }
}

impl<'a> Iterator for LinesWithEndings<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        if self.input.is_empty() {
            return None;
        }
        let split = self.input.find('\n').map(|i| i + 1).unwrap_or(self.input.len());
        let (line, rest) = self.input.split_at(split);
        self.input = rest;
        Some(line)
    }
}