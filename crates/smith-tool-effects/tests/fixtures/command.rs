//! Synthetic controlled commands for sandbox qualification, never a host shell.
use std::{fs, io::Write, time::Duration};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("echo") => println!("{}", args[2..].join(" ")),
        Some("abort") => std::process::abort(),
        Some("write") => {
            fs::write(&args[2], args[3].as_bytes()).unwrap();
            println!("written");
        }
        Some("read") => match fs::read_to_string(&args[2]) {
            Ok(s) => println!("{s}"),
            Err(_) => {
                eprintln!("read denied");
                std::process::exit(23);
            }
        },
        Some("env") => {
            for (key, value) in std::env::vars() {
                println!("{key}={value}");
            }
        }
        Some("fd") => {
            let fd: i32 = args[2].parse().unwrap();
            println!("fd_open={}", unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        }
        Some("network") => {
            let result = std::net::TcpStream::connect_timeout(
                &args[2].parse().unwrap(),
                Duration::from_millis(100),
            );
            println!("connected={}", result.is_ok());
        }
        Some("fork") => {
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                unsafe { libc::_exit(0) }
            }
            println!("fork={pid}");
            if pid > 0 {
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
            }
        }
        Some("sleep") => {
            fs::write("child.pid", std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("spam") => {
            let data = vec![b'x'; 8192];
            loop {
                if std::io::stdout().write_all(&data).is_err() {
                    break;
                }
            }
        }
        Some("symlink-read") => {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&args[2], "link").unwrap();
                match fs::read_to_string("link") {
                    Ok(s) => println!("{s}"),
                    Err(_) => {
                        eprintln!("read denied");
                        std::process::exit(23);
                    }
                }
            }
        }
        _ => std::process::exit(2),
    }
}
