use super::*;

/// Expected CIDs are what Kubo 0.40.1 prints for
/// `ipfs add --only-hash --cid-version=1 --raw-leaves --chunker=size-<chunk>` over [`pattern`].
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

fn import(len: usize, chunk_bytes: usize) -> ImportedFile {
    let params = ImportParams {
        chunk_bytes,
        max_links: ImportParams::PUBLISHED.max_links,
    };
    import_file(pattern(len).as_slice(), params).unwrap()
}

/// Hands out at most seven bytes per call, as a pipe or socket might.
struct Trickle<'a>(&'a [u8]);

impl Read for Trickle<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.0.len().min(buffer.len()).min(7);
        let (head, tail) = self.0.split_at(count);
        buffer[..count].copy_from_slice(head);
        self.0 = tail;
        Ok(count)
    }
}

#[test]
fn files_of_at_most_one_chunk_are_a_single_raw_leaf() {
    for (len, cid) in [
        (
            0,
            "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku",
        ),
        (
            1,
            "bafkreigkgwdvr5wspzwpiutssn4xpj2i7wedshnwphhnu7ohx4pqaxxipe",
        ),
        (
            1024,
            "bafkreienzxhsjvpotyrcx27un4nzhnnzeolqemh2ztj44juk37jr4pxrt4",
        ),
    ] {
        let file = import(len, 1024);
        assert_eq!(file.cid, cid, "{len} bytes");
        assert_eq!(file.size_bytes, len as u64);
        assert_eq!(file.dag_bytes, len as u64);
    }
    let published = import_file(pattern(968_833).as_slice(), ImportParams::PUBLISHED).unwrap();
    assert_eq!(
        published.cid,
        "bafkreigisfh7ddlmfqmvjzwxgt3qs3g6aognr5c4dmays4lt4obvv3qxla"
    );
}

#[test]
fn multi_chunk_files_match_the_balanced_layout_at_every_depth() {
    for (len, chunk, cid) in [
        (
            1025,
            1024,
            "bafybeihw55l5oz7ybhcvoudu6y7chrnrnf5s2nawc6utwbecealp2hlvje",
        ),
        // Exactly one full node of 174 leaves, then one leaf more, which adds a level.
        (
            178_176,
            1024,
            "bafybeibubqudqhkpsrduxv4txqq24pa3xw7rysuarg4hezmfxbyem4chuq",
        ),
        (
            178_177,
            1024,
            "bafybeihiett7xaakvojjyrcxcpizcepdmllshyoxqq6wirddsshlyh3dfe",
        ),
        (
            200_000,
            1024,
            "bafybeidgm2stcydx2vdzk3rwnph6ussbhq6sxi3an7u4f4wheznzzanxoa",
        ),
        // 174 * 174 leaves fill two levels; one more leaf needs a third.
        (
            968_832,
            32,
            "bafybeih5pqs4pyuhpu7mwar6tvcfzslx6xzb5w2i32xfwm6xyigcn65n5m",
        ),
        (
            968_833,
            32,
            "bafybeienfovsrtbazxpty3qgd7cvbwogjz2u6hkvyhqjj2b6yx6vufx3gy",
        ),
    ] {
        let file = import(len, chunk);
        assert_eq!(file.cid, cid, "{len} bytes in {chunk}-byte chunks");
        assert_eq!(file.size_bytes, len as u64);
        assert!(file.dag_bytes > len as u64);
    }
}

#[test]
fn short_reads_do_not_move_chunk_boundaries() {
    let bytes = pattern(200_000);
    let params = ImportParams {
        chunk_bytes: 1024,
        max_links: 174,
    };
    let trickled = import_file(Trickle(&bytes), params).unwrap();
    assert_eq!(trickled, import_file(bytes.as_slice(), params).unwrap());
}

#[test]
fn reports_the_sha256_of_the_file_bytes() {
    assert_eq!(
        import(0, 1024).sha256_hex,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        import(200_000, 1024).sha256_hex,
        format!("{:x}", Sha256::digest(pattern(200_000)))
    );
}

fn entry(name: &str, file: &ImportedFile) -> DirectoryEntry {
    DirectoryEntry {
        name: name.to_owned(),
        cid: file.cid.clone(),
        dag_bytes: file.dag_bytes,
    }
}

#[test]
fn directory_matches_kubo_whatever_the_entry_order() {
    // `ipfs add -r` of a directory holding these three files.
    let expected = "bafybeifgg3gj5yrrb6yltnoygqwjzrmasqjb5nrysl7qepaqmj3ilzdhaa";
    let entries = [
        entry("empty", &import(0, 1024)),
        entry("b.bin", &import(1, 1024)),
        entry("a.pmtiles", &import(200_000, 1024)),
    ];
    let node = directory_node(&entries).unwrap();
    assert_eq!(node.cid, expected);
    let mut reversed = entries.to_vec();
    reversed.reverse();
    assert_eq!(directory_node(&reversed).unwrap(), node);
    assert_eq!(cid_string(&cid_bytes(CODEC_DAG_PB, &node.block)), expected);
}

#[test]
fn directory_of_published_archives_matches_the_pinned_one() {
    // Root CIDs, sizes and link sizes of three archives as a Kubo node holding them lists them.
    let entries = [
        DirectoryEntry {
            name: "airspace.pmtiles".to_owned(),
            cid: "bafybeif2ky23uspn5ubmudfvnlxjojvz3jhjdwvpeazhdogfhyuwevrddu".to_owned(),
            dag_bytes: 28_259_768,
        },
        DirectoryEntry {
            name: "ifr_high.pmtiles".to_owned(),
            cid: "bafybeie6rdyalstlzktelcff5ypr4u4rqgifofucfj3jrppxj6enzvdt4y".to_owned(),
            dag_bytes: 73_315_060,
        },
        DirectoryEntry {
            name: "vfr_sectional.pmtiles".to_owned(),
            cid: "bafybeihrsxpqx5wucehht7ubj4ipcwdu73jxexa6u4bf6bcosozm7pvony".to_owned(),
            dag_bytes: 1_235_385_838,
        },
    ];
    assert_eq!(
        directory_node(&entries).unwrap().cid,
        "bafybeidhfzynen2lnbbfdg6earhkw47eyo6zizifhqgiuoxiyahwfrnct4"
    );
}

#[test]
fn rejects_what_cannot_be_a_directory_entry() {
    let file = import(1, 1024);
    for name in ["", "a/b"] {
        assert!(matches!(
            directory_node(&[entry(name, &file)]),
            Err(UnixfsError::EntryName(_))
        ));
    }
    assert!(matches!(
        directory_node(&[entry("a", &file), entry("a", &file)]),
        Err(UnixfsError::DuplicateEntry(_))
    ));
    let cidv0 = DirectoryEntry {
        name: "a".to_owned(),
        cid: "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG".to_owned(),
        dag_bytes: 1,
    };
    assert!(matches!(directory_node(&[cidv0]), Err(UnixfsError::Cid(_))));
    let mut truncated = entry("a", &file);
    truncated.cid.pop();
    assert!(matches!(
        directory_node(&[truncated]),
        Err(UnixfsError::Cid(_))
    ));
}

#[test]
fn cid_text_round_trips() {
    let file = import(200_000, 1024);
    let bytes = parse_cid(&file.cid).unwrap();
    assert_eq!(cid_string(&bytes), file.cid);
}

#[test]
fn degenerate_parameters_are_refused() {
    for params in [
        ImportParams {
            chunk_bytes: 0,
            max_links: 174,
        },
        ImportParams {
            chunk_bytes: 1024,
            max_links: 1,
        },
    ] {
        assert!(matches!(
            import_file([0_u8; 4].as_slice(), params),
            Err(UnixfsError::Params)
        ));
    }
}
