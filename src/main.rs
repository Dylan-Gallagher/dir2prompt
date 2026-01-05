use clap::Parser;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "dir2prompt",
    about = "Dump a directory as Markdown for LLM prompting (respects .gitignore)."
)]
struct Args {
    /// Root directory to dump
    #[arg(default_value = ".")]
    root: PathBuf,

    /// Max bytes to include per file (files are truncated beyond this)
    #[arg(long, default_value_t = 200_000)]
    max_bytes: usize,

    /// If set, do NOT respect .gitignore / git excludes / global ignores
    #[arg(long)]
    no_gitignore: bool,

    /// If set, exclude hidden files/dirs (dotfiles)
    #[arg(long)]
    no_hidden: bool,

    /// If set, include common lockfiles (Cargo.lock, package-lock.json, etc.)
    #[arg(long)]
    include_lockfiles: bool,

    /// Additional exclude globs (gitignore-style), may be repeated
    ///
    /// Examples:
    ///   --exclude '**/*.snap'
    ///   --exclude '**/generated/**'
    #[arg(long)]
    exclude: Vec<String>,

    /// Additional include globs (gitignore-style), may be repeated.
    /// These act as "force include" overrides using `!glob`.
    ///
    /// Example:
    ///   --include '**/Cargo.lock'
    #[arg(long)]
    include: Vec<String>,

    /// If set, skip files that are not valid UTF-8 (instead of lossy output)
    #[arg(long)]
    strict_utf8: bool,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let root = normalize_root(&args.root)?;

    let respect_gitignore = !args.no_gitignore;

    let overrides = build_overrides(&root, args.include_lockfiles, &args.exclude, &args.include)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let mut walk = WalkBuilder::new(&root);
    walk.overrides(overrides);

    // Hidden handling: default is to include hidden (dotfiles), unless --no_hidden
    walk.hidden(args.no_hidden);

    // Respect gitignore & related mechanisms unless --no-gitignore
    walk.git_ignore(respect_gitignore);
    walk.git_exclude(respect_gitignore);
    walk.git_global(respect_gitignore);
    walk.parents(respect_gitignore);

    // Also respect `.ignore` files (ripgrep style) when honoring ignore rules
    walk.ignore(respect_gitignore);

    // Don’t follow symlinks by default (safer, avoids cycles)
    walk.follow_links(false);

    let mut files: Vec<PathBuf> = Vec::new();
    for result in walk.build() {
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                eprintln!("dir2prompt: walk error: {err}");
                continue;
            }
        };

        let ft = match entry.file_type() {
            Some(t) => t,
            None => continue,
        };

        if !ft.is_file() {
            continue;
        }

        files.push(entry.into_path());
    }

    files.sort();

    println!("# dir2prompt dump");
    println!();
    println!("- Root: `{}`", root.display());
    println!(
        "- Respect .gitignore: `{}`",
        if respect_gitignore { "yes" } else { "no" }
    );
    println!(
        "- Hidden files included: `{}`",
        if args.no_hidden { "no" } else { "yes" }
    );
    println!("- Per-file max bytes: `{}`", args.max_bytes);
    println!();
    println!("## Included files");
    for path in &files {
        let rel = rel_path(&root, path);
        println!("- `{}`", rel.display());
    }
    println!();
    println!("---");
    println!();

    let mut printed = 0usize;
    let mut skipped_binary = 0usize;
    let mut skipped_utf8 = 0usize;

    for path in &files {
        let rel = rel_path(&root, path);
        let lang = language_tag(path);

        match read_file_limited(path, args.max_bytes) {
            Ok(ReadResult { bytes, truncated }) => {
                if looks_binary(&bytes) {
                    skipped_binary += 1;
                    println!("## `{}`", rel.display());
                    println!();
                    println!("(skipped: looks like a binary file)");
                    println!();
                    continue;
                }

                let (text, utf8_note) = bytes_to_text(&bytes, args.strict_utf8);
                let Some(text) = text else {
                    skipped_utf8 += 1;
                    println!("## `{}`", rel.display());
                    println!();
                    println!("(skipped: not valid UTF-8)");
                    println!();
                    continue;
                };

                println!("## `{}`", rel.display());
                println!();

                if truncated {
                    println!("(truncated to {} bytes)", args.max_bytes);
                    println!();
                }
                if let Some(note) = utf8_note {
                    println!("({note})");
                    println!();
                }

                println!("```{}", lang);
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
                println!("```");
                println!();

                printed += 1;
            }
            Err(err) => {
                println!("## `{}`", rel.display());
                println!();
                println!("(skipped: failed to read file: {err})");
                println!();
            }
        }
    }

    eprintln!(
        "dir2prompt: printed {printed} files, skipped binary {skipped_binary}, \
skipped utf8 {skipped_utf8}"
    );

    Ok(())
}

fn normalize_root(root: &Path) -> io::Result<PathBuf> {
    let root = if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root.to_path_buf()
    };
    // Canonicalize if possible; fall back to provided path if it fails.
    match std::fs::canonicalize(&root) {
        Ok(p) => Ok(p),
        Err(_) => Ok(root),
    }
}

fn rel_path<'a>(root: &'a Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

fn build_overrides(
    root: &Path,
    include_lockfiles: bool,
    excludes: &[String],
    includes: &[String],
) -> Result<ignore::overrides::Override, String> {
    let mut ob = OverrideBuilder::new(root);

    // Always skip VCS dirs (even if someone disables gitignore respecting).
    // (The walker already has behavior around .git, but this makes it explicit.)
    add_exclude(&mut ob, "**/.git/**")?;
    add_exclude(&mut ob, "**/.hg/**")?;
    add_exclude(&mut ob, "**/.svn/**")?;

    // Common virtualenv / cache / build artifacts
    add_exclude(&mut ob, "**/.venv/**")?;
    add_exclude(&mut ob, "**/venv/**")?;
    add_exclude(&mut ob, "**/__pycache__/**")?;
    add_exclude(&mut ob, "**/.mypy_cache/**")?;
    add_exclude(&mut ob, "**/.pytest_cache/**")?;
    add_exclude(&mut ob, "**/.ruff_cache/**")?;
    add_exclude(&mut ob, "**/.tox/**")?;

    // Common dependency/build output dirs
    add_exclude(&mut ob, "**/node_modules/**")?;
    add_exclude(&mut ob, "**/target/**")?;
    add_exclude(&mut ob, "**/dist/**")?;
    add_exclude(&mut ob, "**/build/**")?;
    add_exclude(&mut ob, "**/.next/**")?;
    add_exclude(&mut ob, "**/.nuxt/**")?;
    add_exclude(&mut ob, "**/.svelte-kit/**")?;

    // OS/editor noise
    add_exclude(&mut ob, "**/.DS_Store")?;
    add_exclude(&mut ob, "**/Thumbs.db")?;

    // “Package files” / lockfiles (skip by default; can be re-enabled)
    if !include_lockfiles {
        add_exclude(&mut ob, "**/Cargo.lock")?;
        add_exclude(&mut ob, "**/package-lock.json")?;
        add_exclude(&mut ob, "**/yarn.lock")?;
        add_exclude(&mut ob, "**/pnpm-lock.yaml")?;
        add_exclude(&mut ob, "**/composer.lock")?;
        add_exclude(&mut ob, "**/Gemfile.lock")?;
        add_exclude(&mut ob, "**/poetry.lock")?;
        add_exclude(&mut ob, "**/Pipfile.lock")?;
    }

    for ex in excludes {
        add_exclude(&mut ob, ex)?;
    }

    // Includes are "negated" patterns in override syntax.
    for inc in includes {
        let line = if inc.starts_with('!') {
            inc.clone()
        } else {
            format!("!{inc}")
        };
        add_exclude(&mut ob, &line)?;
    }

    ob.build().map_err(|e| e.to_string())
}

fn add_exclude(ob: &mut OverrideBuilder, pattern: &str) -> Result<(), String> {
    let p = pattern.trim();
    let line = if p.starts_with('!') {
        p.to_string()
    } else {
        format!("!{p}")
    };
    ob.add(&line)
        .map_err(|e| format!("bad override '{line}': {e}"))?;
    Ok(())
}

fn add_include(ob: &mut OverrideBuilder, pattern: &str) -> Result<(), String> {
    let p = pattern.trim();
    let line = p.strip_prefix('!').unwrap_or(p);
    ob.add(line)
        .map_err(|e| format!("bad override '{line}': {e}"))?;
    Ok(())
}

struct ReadResult {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_file_limited(path: &Path, max_bytes: usize) -> io::Result<ReadResult> {
    let f = File::open(path)?;
    let mut buf = Vec::with_capacity(std::cmp::min(max_bytes, 64 * 1024));

    let mut limited = f.take((max_bytes as u64) + 1);
    limited.read_to_end(&mut buf)?;

    let truncated = buf.len() > max_bytes;
    if truncated {
        buf.truncate(max_bytes);
    }

    Ok(ReadResult {
        bytes: buf,
        truncated,
    })
}

fn looks_binary(bytes: &[u8]) -> bool {
    // Heuristic: if the first chunk contains a NUL byte, treat as binary.
    let n = std::cmp::min(bytes.len(), 8 * 1024);
    bytes[..n].iter().any(|&b| b == 0)
}

fn bytes_to_text(bytes: &[u8], strict_utf8: bool) -> (Option<String>, Option<&'static str>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (Some(s.to_string()), None),
        Err(_) if strict_utf8 => (None, None),
        Err(_) => (
            Some(String::from_utf8_lossy(bytes).to_string()),
            Some("note: contained invalid UTF-8; printed with lossless replacement"),
        ),
    }
}

fn language_tag(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "md" => "markdown",
        "txt" => "text",
        "json" => "json",
        "yml" | "yaml" => "yaml",
        "js" => "javascript",
        "ts" => "ts",
        "jsx" => "jsx",
        "tsx" => "tsx",
        "py" => "python",
        "sh" => "bash",
        "zsh" => "zsh",
        "fish" => "fish",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "html" => "html",
        "css" => "css",
        "scss" => "scss",
        "proto" => "proto",
        "ini" => "ini",
        "env" => "bash",
        _ => "text",
    }
}
