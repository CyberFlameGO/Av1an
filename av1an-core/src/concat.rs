use anyhow::{anyhow, Context};
use av_format::{
  buffer::AccReader,
  demuxer::{Context as DemuxerContext, Event},
  muxer::Context as MuxerContext,
};
use av_ivf::{demuxer::IvfDemuxer, muxer::IvfMuxer};
use path_abs::PathAbs;
use serde::{Deserialize, Serialize};
use std::{
  fmt::Display,
  fs::{self, DirEntry, File},
  io::Write,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  sync::Arc,
};

#[derive(
  PartialEq, Eq, Copy, Clone, Serialize, Deserialize, Debug, strum::EnumString, strum::IntoStaticStr,
)]
pub enum ConcatMethod {
  #[strum(serialize = "mkvmerge")]
  MKVMerge,
  #[strum(serialize = "ffmpeg")]
  FFmpeg,
  #[strum(serialize = "ivf")]
  Ivf,
}

impl Display for ConcatMethod {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(<&'static str>::from(self))
  }
}

pub fn sort_files_by_filename(files: &mut [PathBuf]) {
  files.sort_unstable_by_key(|x| {
    // If the temp directory follows the expected format of 00000.ivf, 00001.ivf, etc.,
    // then these unwraps will not fail
    x.file_stem()
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<u32>()
      .unwrap()
  });
}

pub fn ivf(input: &Path, out: &Path) -> anyhow::Result<()> {
  let mut files: Vec<PathBuf> = fs::read_dir(input)?
    .into_iter()
    .filter_map(Result::ok)
    .filter_map(|d| {
      if let Ok(file_type) = d.file_type() {
        if file_type.is_file() {
          Some(d.path())
        } else {
          None
        }
      } else {
        None
      }
    })
    .collect();

  sort_files_by_filename(&mut files);

  assert!(!files.is_empty());

  let output = File::create(out)?;

  let mut muxer = MuxerContext::new(Box::new(IvfMuxer::new()), Box::new(output));

  let global_info = {
    let acc = AccReader::new(std::fs::File::open(&files[0]).unwrap());
    let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));

    demuxer.read_headers().unwrap();

    // attempt to set the duration correctly
    let duration = demuxer.info.duration.unwrap_or(0)
      + files
        .iter()
        .skip(1)
        .filter_map(|file| {
          let acc = AccReader::new(std::fs::File::open(file).unwrap());
          let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));

          demuxer.read_headers().unwrap();
          demuxer.info.duration
        })
        .sum::<u64>();

    let mut info = demuxer.info;
    info.duration = Some(duration);
    info
  };

  muxer.set_global_info(global_info)?;

  muxer.configure()?;
  muxer.write_header()?;

  let mut pos_offset: usize = 0;
  for file in &files {
    let mut last_pos: usize = 0;
    let input = std::fs::File::open(file)?;

    let acc = AccReader::new(input);

    let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));
    demuxer.read_headers()?;

    trace!("global info: {:#?}", demuxer.info);

    loop {
      match demuxer.read_event() {
        Ok(event) => match event {
          Event::MoreDataNeeded(sz) => panic!("needed more data: {} bytes", sz),
          Event::NewStream(s) => panic!("new stream: {:?}", s),
          Event::NewPacket(mut packet) => {
            if let Some(p) = packet.pos.as_mut() {
              last_pos = *p;
              *p += pos_offset;
            }

            debug!("received packet with pos: {:?}", packet.pos);
            muxer.write_packet(Arc::new(packet))?;
          }
          Event::Continue => continue,
          Event::Eof => {
            debug!("EOF received.");
            break;
          }
          _ => unimplemented!(),
        },
        Err(e) => {
          debug!("error: {:?}", e);
          break;
        }
      }
    }
    pos_offset += last_pos + 1;
  }

  muxer.write_trailer()?;

  Ok(())
}

fn read_encoded_chunks(encode_dir: &Path) -> anyhow::Result<Vec<DirEntry>> {
  Ok(
    fs::read_dir(&encode_dir)
      .with_context(|| format!("Failed to read encoded chunks from {:?}", &encode_dir))?
      .collect::<Result<Vec<_>, _>>()?,
  )
}

pub fn mkvmerge(encode_folder: &Path, output: &Path) -> anyhow::Result<()> {
  /// Path cannot be a UNC path on Windows for mkvmerge
  #[cfg(windows)]
  fn fix_path<P: AsRef<Path>>(p: P) -> String {
    const UNC_PREFIX: &str = r#"\\?\"#;

    let p = p.as_ref().display().to_string();
    if p.starts_with(UNC_PREFIX) {
      p[UNC_PREFIX.len()..].to_string()
    } else {
      p
    }
  }

  #[cfg(not(windows))]
  fn fix_path<P: AsRef<Path>>(p: P) -> P {
    p
  }

  let mut encode_folder = PathBuf::from(encode_folder);

  let mut audio_file = PathBuf::from(&encode_folder);
  audio_file.push("audio.mkv");

  encode_folder.push("encode");
  let output = PathBuf::from(output);

  let mut files = read_encoded_chunks(&encode_folder)?;

  assert!(!files.is_empty());

  files.sort_by_key(DirEntry::path);

  let mut cmd = Command::new("mkvmerge");
  cmd.arg("-o");
  cmd.arg(fix_path(output));

  if audio_file.exists() {
    cmd.arg(fix_path(audio_file));
  }

  // `std::process::Command` does not support removing arguments after they have been added,
  // so we have to add all elements without adding any extra that are later removed. This
  // complicates the logic slightly, but in turn does not perform any unnecessary allocations
  // or copy from a temporary data structure.
  if files.len() % 2 != 0 {
    let mut chunks = files.chunks_exact(2);
    for files in &mut chunks {
      // Each chunk always has exactly 2 elements.
      for file in files {
        cmd.arg(fix_path(file.path()));
        cmd.arg("+");
      }
    }
    // The remainder is always *exactly* one element since we are using `chunks_exact(2)`, and we
    // asserted that the length of `files` is odd and nonzero in this branch.
    cmd.arg(fix_path(chunks.remainder()[0].path()));
  } else {
    // The total number of elements at this point is even, and there are at *least* 2 elements,
    // since `files` is not empty and the case of exactly one element would have been handled by
    // the previous `if`, so we get the last 2 to handle them separately from the other pairs of 2,
    // so as to not add a trailing "+" at the end.

    // `files.len() - 2` cannot overflow, as `files.len()` is at least 2 here.
    let (start, end) = files.split_at(files.len() - 2);

    // `start` will be empty if there are exactly 2 elements in `files`, in which case
    // this loop will not run.
    for file in start {
      cmd.arg(fix_path(file.path()));
      cmd.arg("+");
    }

    // There are always *exactly* 2 elements in `end`, since we used `split_at(files.len() - 2)`,
    // which will always succeed given that `files` has at least 2 elements at this point.
    cmd.arg(fix_path(end[0].path()));
    cmd.arg("+");
    cmd.arg(fix_path(end[1].path()));
  }

  debug!("mkvmerge concat command: {:?}", cmd);

  let out = cmd
    .output()
    .with_context(|| "Failed to execute mkvmerge command for concatenation")?;

  if !out.status.success() {
    // TODO: make an EncoderCrash-like struct, but without all the other fields so it
    // can be used in a more broad scope than just for the pipe/encoder
    error!(
      "mkvmerge concatenation failed with output: {:#?}\ncommand: {:?}",
      out, cmd
    );
    return Err(anyhow!("mkvmerge concatenation failed"));
  }

  Ok(())
}

/// Concatenates using ffmpeg (does not work with x265)
pub fn ffmpeg(temp: &Path, output: &Path) -> anyhow::Result<()> {
  fn write_concat_file(temp_folder: &Path) -> anyhow::Result<()> {
    let concat_file = temp_folder.join("concat");
    let encode_folder = temp_folder.join("encode");

    let mut files = read_encoded_chunks(&encode_folder)?;

    files.sort_by_key(DirEntry::path);

    let mut contents = String::with_capacity(24 * files.len());

    for i in files {
      contents.push_str(&format!(
        "file {}\n",
        format!("{}", i.path().display())
          .replace('\\', r"\\")
          .replace(' ', r"\ ")
          .replace('\'', r"\'")
      ));
    }

    let mut file = File::create(concat_file)?;
    file.write_all(contents.as_bytes())?;

    Ok(())
  }

  let temp = PathAbs::new(temp)?;
  let temp = temp.as_path();

  let concat = temp.join("concat");
  let concat_file = concat.to_str().unwrap();

  write_concat_file(temp)?;

  let audio_file = {
    let file = temp.join("audio.mkv");
    if file.exists() && file.metadata().unwrap().len() > 1000 {
      Some(file)
    } else {
      None
    }
  };

  let mut cmd = Command::new("ffmpeg");

  cmd.stdout(Stdio::piped());
  cmd.stderr(Stdio::piped());

  if let Some(file) = audio_file {
    cmd
      .args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        concat_file,
        "-i",
      ])
      .arg(file)
      .args(["-map", "0", "-map", "1", "-c", "copy"])
      .arg(output);
  } else {
    cmd
      .args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        concat_file,
      ])
      .args(["-map", "0", "-c", "copy"])
      .arg(output);
  }

  debug!("FFmpeg concat command: {:?}", cmd);

  let out = cmd
    .output()
    .with_context(|| "Failed to execute FFmpeg command for concatenation")?;

  if !out.status.success() {
    error!(
      "FFmpeg concatenation failed with output: {:#?}\ncommand: {:?}",
      out, cmd
    );
    return Err(anyhow!("FFmpeg concatenation failed"));
  }

  Ok(())
}
