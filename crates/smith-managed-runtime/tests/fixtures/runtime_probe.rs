use std::{io::Write, time::Duration};
fn main() {
    use std::os::unix::fs::MetadataExt;
    let exe = std::env::current_exe().unwrap();
    let cwd = std::fs::metadata(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(cwd.mode() & 0o777, 0o700);
    assert_eq!(cwd.uid(), std::fs::metadata(&exe).unwrap().uid());
    let mode = exe.file_stem().unwrap().to_str().unwrap();
    std::fs::write(exe.with_extension("pid"), std::process::id().to_string()).unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 1);
    assert!(std::env::vars().all(|(key, _)| key == "LANG"));
    match mode {
        "stall" => std::thread::sleep(Duration::from_secs(60)),
        "flood" => {
            std::io::stdout().write_all(&vec![b'x'; 300_000]).ok();
            std::thread::sleep(Duration::from_secs(60));
        }
        "invalid" => {
            std::io::stdout().write_all(&[255]).unwrap();
            return;
        }
        "nonzero" => std::process::exit(3),
        _ => {}
    }
    if args[0] == "--help" {
        println!("--model FILE\n--alias ID\n--host HOST unix socket paths\n--port N\n--parallel N\n--offline\n--no-agent\n--no-webui\n--no-mmproj\n--fit [on|off]\n--device NAME none\n--n-gpu-layers N\n--ctx-size N\n--threads N\n--api-key KEY\n--threads-batch N\n--batch-size N\n--ubatch-size N\n--flash-attn [on|off|auto]\n--cache-type-k TYPE f16 q8_0 q4_0\n--cache-type-v TYPE f16 q8_0 q4_0\n--load-mode MODE none mmap mlock mmap+mlock");
    } else if args[0] == "--list-devices" {
        println!("irrelevant CUDA outside table\nAvailable devices:\n  CPU: Synthetic CPU\n  Metal0: Synthetic Metal (1024 MiB, 512 MiB free)\n  CUDA0: Synthetic CUDA\n  HIP0: Synthetic HIP\n  SYCL0: Synthetic Intel\n  Metal0: repeated\n  not valid: ignored");
    } else {
        panic!("unexpected argument")
    }
}
