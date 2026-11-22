//! Streaming copy and byte-for-byte verify with progress, for the PKG installer. Ported from
//! TITAN CORE (installer_io.rs, GPL-3.0-or-later); see CREDITS.md.
use std::{fs, io::{self, Read, Write}, path::Path};

pub fn transfer(input: &mut impl Read, output: &mut impl Write, total: u64, mut progress: impl FnMut(u64) -> io::Result<()>) -> io::Result<()> {
    let mut buffer = vec![0; 1 << 20];
    let mut done = 0;
    loop {
        progress(done)?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if done + n as u64 > total {
            return Err(io::Error::other("Source grew during copy"));
        }
        output.write_all(&buffer[..n])?;
        done += n as u64;
    }
    if done != total {
        return Err(io::Error::other("Source was truncated during copy"));
    }
    progress(done)
}

pub fn verify(a: &Path, b: &Path, total: u64, mut progress: impl FnMut(u64) -> io::Result<()>) -> io::Result<()> {
    let mut a = fs::File::open(a)?;
    let mut b = fs::File::open(b)?;
    let mut x = vec![0; 1 << 20];
    let mut y = vec![0; 1 << 20];
    let mut done = 0;
    while done < total {
        progress(done)?;
        let n = ((total - done) as usize).min(x.len());
        a.read_exact(&mut x[..n])?;
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Err(io::Error::other("Copied package verification failed"));
        }
        done += n as u64;
    }
    if a.read(&mut x[..1])? != 0 || b.read(&mut y[..1])? != 0 {
        return Err(io::Error::other("Package size changed"));
    }
    progress(done)
}
