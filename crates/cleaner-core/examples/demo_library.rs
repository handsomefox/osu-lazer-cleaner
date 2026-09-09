//! Builds a library to take screenshots against.
//!
//! The real library is someone's personal collection, so the pictures in the readme are taken
//! against this instead. Blobs are written as sparse files: the sizes the window reports are
//! the sizes on disk, but the directory costs a few hundred kilobytes rather than the hundred
//! gigabytes it claims.
//!
//! ```
//! cargo run -p cleaner-core --example demo_library -- /tmp/demo
//! ```

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a developer tool reports what it did"
)]

use cleaner_core::Library;
use cleaner_realm::fixture::{SetFixture, synthetic_realm};
use std::io::Write as _;

/// How many beatmap sets to invent.
const SETS: usize = 240;

/// Artists to draw from, so the browse list reads like a library rather than a test.
const ARTISTS: &[&str] = &[
    "Camellia",
    "xi",
    "Nekomata Master",
    "cYsmix",
    "LeaF",
    "DragonForce",
    "Yooh",
    "USAO",
    "Kobaryo",
    "t+pazolite",
];

/// Titles to draw from.
const TITLES: &[&str] = &[
    "Ghost",
    "Blue Zenith",
    "Scarlet Rose",
    "triangles",
    "Aleph-0",
    "Through the Fire and Flames",
    "Amenohoakari",
    "Bass Slut",
    "Denpa Yousai",
    "Oshama Scramble!",
];

/// A blob to write, with the size the window will report for it.
struct Blob {
    hash: String,
    bytes: u64,
    /// Content, for the files a scan has to parse. Sparse otherwise.
    text: Option<String>,
}

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let Some(root) = arguments.next() else {
        eprintln!("usage: demo_library <directory>");
        std::process::exit(2);
    };
    let root = std::path::PathBuf::from(root);
    std::fs::create_dir_all(&root).expect("could not create the directory");

    let mut sets = Vec::with_capacity(SETS);
    let mut blobs = Vec::new();
    for index in 0..SETS {
        let (set, mut written) = build_set(index);
        sets.push(set);
        blobs.append(&mut written);
    }

    // The realm handle has to close before anything else opens the database.
    drop(synthetic_realm(&root.join("client.realm"), &sets));

    let library = Library::open(&root).expect("could not open the library just written");
    for blob in &blobs {
        write_blob(&library, blob);
    }

    println!(
        "wrote {SETS} sets and {} files to {}",
        blobs.len(),
        root.display()
    );
}

/// Invents one set and the files it owns.
fn build_set(index: usize) -> (SetFixture, Vec<Blob>) {
    let artist = ARTISTS[index % ARTISTS.len()];
    let title = TITLES[(index / ARTISTS.len()) % TITLES.len()];
    let difficulty = format!("{artist} - {title} [Insane].osu");

    // Every file needs its own hash, or the sets would share blobs and the totals would count
    // each of them once. A hash is hex and nothing else: a stray letter outside `0-9a-f` makes
    // a path no blob is stored under, and every size comes back as zero.
    let hash = |kind: u32| format!("{:056x}{kind:08x}", index + 1);

    let mut files = vec![
        (difficulty.clone(), hash(0)),
        ("audio.mp3".to_owned(), hash(1)),
        ("bg.jpg".to_owned(), hash(2)),
        ("hitcircle.png".to_owned(), hash(3)),
        ("soft-hitwhistle.wav".to_owned(), hash(4)),
        ("Thumbs.db".to_owned(), hash(5)),
    ];
    let mut blobs = vec![
        Blob {
            hash: hash(0),
            bytes: 0,
            text: Some(difficulty_text()),
        },
        Blob {
            hash: hash(1),
            bytes: 6_800_000,
            text: None,
        },
        Blob {
            hash: hash(2),
            bytes: 1_400_000,
            text: None,
        },
        Blob {
            hash: hash(3),
            bytes: 180_000,
            text: None,
        },
        Blob {
            hash: hash(4),
            bytes: 240_000,
            text: None,
        },
        Blob {
            hash: hash(5),
            bytes: 12_288,
            text: None,
        },
    ];

    // Not every set has a video or a storyboard, which is what makes the categories worth
    // showing separately.
    if index.is_multiple_of(3) {
        files.push(("intro.mp4".to_owned(), hash(6)));
        blobs.push(Blob {
            hash: hash(6),
            bytes: 42_000_000,
            text: None,
        });
    }
    if index.is_multiple_of(2) {
        files.push((format!("{artist} - {title}.osb"), hash(7)));
        files.push(("sb/sprite.png".to_owned(), hash(8)));
        blobs.push(Blob {
            hash: hash(7),
            bytes: 0,
            text: Some(storyboard_text()),
        });
        blobs.push(Blob {
            hash: hash(8),
            bytes: 900_000,
            text: None,
        });
    }

    let mut set = SetFixture::new(
        id(index),
        title,
        &files
            .iter()
            .map(|(name, hash)| (name.as_str(), hash.as_str()))
            .collect::<Vec<_>>(),
    );
    artist.clone_into(&mut set.artist);
    "audio.mp3".clone_into(&mut set.audio);
    "bg.jpg".clone_into(&mut set.background);
    (set, blobs)
}

/// A set's primary key, which only has to be unique.
fn id(index: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
    bytes
}

/// A difficulty naming the background and a custom hitsound, which is what the scan reads.
fn difficulty_text() -> String {
    "osu file format v14\n\n\
     [General]\n\
     AudioFilename: audio.mp3\n\n\
     [Events]\n\
     0,0,\"bg.jpg\",0,0\n\
     Video,0,\"intro.mp4\"\n\n\
     [TimingPoints]\n\
     0,500,4,2,1,60,1,0\n\n\
     [HitObjects]\n\
     256,192,1000,1,0,0:0:0:0:soft-hitwhistle.wav\n"
        .to_owned()
}

/// A storyboard drawing one sprite.
fn storyboard_text() -> String {
    "[Events]\n\
     //Storyboard Layer 0 (Background)\n\
     Sprite,Background,Centre,\"sb/sprite.png\",320,240\n"
        .to_owned()
}

/// Writes one blob, sparse unless it has content a scan has to read.
fn write_blob(library: &Library, blob: &Blob) {
    let path = library.blob_path(&blob.hash);
    std::fs::create_dir_all(path.parent().expect("a blob path has a parent"))
        .expect("could not create the blob directory");
    let mut file = std::fs::File::create(&path).expect("could not write a blob");
    match &blob.text {
        Some(text) => file
            .write_all(text.as_bytes())
            .expect("could not write a blob"),
        // Sparse: the length is what the window measures, and no bytes are stored.
        None => file.set_len(blob.bytes).expect("could not size a blob"),
    }
}
