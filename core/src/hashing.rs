//! Хеширование файлов (общее для «сравнения» и «дубликатов»).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use md5::Digest as _; // общий digest::Digest для sha2 и md5 (оба re-export одного trait)

/// Алгоритм хеширования (выбирается в диалоге).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
    Md5,
}

impl HashAlgo {
    pub fn name(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Md5 => "md5",
        }
    }
}

/// Хеш файла по блокам (64 КБ). `None` — файл не читается.
pub fn hash_file(path: &Path, algo: HashAlgo) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 64 * 1024];
    match algo {
        HashAlgo::Sha256 => {
            let mut h = sha2::Sha256::new();
            loop {
                let n = f.read(&mut buf).ok()?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(format!("{:x}", h.finalize()))
        }
        HashAlgo::Md5 => {
            let mut h = md5::Md5::new();
            loop {
                let n = f.read(&mut buf).ok()?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Some(format!("{:x}", h.finalize()))
        }
    }
}

/// Побайтовое сравнение двух файлов:
/// сначала размер, затем блоки по 8192 байта. Ошибка чтения = «различаются».
pub fn files_differ_binary(a: &Path, b: &Path) -> bool {
    let (ma, mb) = match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return true,
    };
    if ma.len() != mb.len() {
        return true;
    }
    let (fa, fb) = match (File::open(a), File::open(b)) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return true,
    };
    let mut fa = std::io::BufReader::new(fa);
    let mut fb = std::io::BufReader::new(fb);
    let mut buf1 = [0u8; 8192];
    let mut buf2 = [0u8; 8192];
    loop {
        let n1 = match fa.read(&mut buf1) {
            Ok(n) => n,
            Err(_) => return true,
        };
        let n2 = match fb.read(&mut buf2) {
            Ok(n) => n,
            Err(_) => return true,
        };
        if buf1[..n1] != buf2[..n2] {
            return true;
        }
        if n1 == 0 {
            break;
        }
    }
    false
}
