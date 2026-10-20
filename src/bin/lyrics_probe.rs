//! Dev tool: `lyrics_probe <artist> <title> [album] [seconds]` prints what the widget would show.
#[path = "../lyrics.rs"]
#[allow(dead_code)]
mod lyrics;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: lyrics_probe <artist> <title> [album] [seconds]");
        return;
    }
    let req = lyrics::Request {
        key: String::new(),
        artist: a[0].clone(),
        title: a[1].clone(),
        album: a.get(2).cloned().unwrap_or_default(),
        duration_ms: a.get(3).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0) * 1000,
    };
    match lyrics::fetch(&req) {
        lyrics::Body::Synced(lines) => {
            println!("synced, {} lines", lines.len());
            for (t, s) in lines.iter().take(8) {
                println!("{:>3}:{:02}.{:03}  {}", t / 60000, t / 1000 % 60, t % 1000, s);
            }
        }
        lyrics::Body::Plain(lines) => {
            println!("plain, {} lines", lines.len());
            for s in lines.iter().take(8) {
                println!("{s}");
            }
        }
        lyrics::Body::Message(m) => println!("message: {m}"),
    }
}
