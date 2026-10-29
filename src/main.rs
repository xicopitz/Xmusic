mod apply;
mod auth;
mod db;
mod job;
mod matcher;
mod metrics;
mod remote;
mod scan;
mod tags;
mod watch;
mod web;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => {
            if let Err(e) = db::check() {
                eprintln!("xmusic: {e}");
                std::process::exit(1);
            }
            std::thread::spawn(job::scheduler);
            std::thread::spawn(watch::run);
            web::serve();
        }
        ["reset-password", user] => {
            let pw = rpassword_free_read();
            match auth::set_password(&db::open(), user, &pw) {
                Ok(()) => println!("password updated for {user}"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: xmusic [serve] | reset-password <user>");
            std::process::exit(2);
        }
    }
}

/// Reads the new password from stdin (echoes; use `docker exec -it` or pipe it in).
fn rpassword_free_read() -> String {
    eprint!("new password: ");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).unwrap();
    s.trim_end_matches(['\r', '\n']).to_string()
}
