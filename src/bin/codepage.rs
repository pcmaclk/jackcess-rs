//! `codepage` - verifies the port's code page support against the JDK.
//!
//! Every mode consumes the reference data `java-src/CodePageGen.java` writes
//! from the real JDK charsets, and reproduces the expected result with the
//! library's [`Charset`].  The harness `tools/verify-codepage.ps1` runs all of
//! it.
//!
//! ```text
//! codepage verify-list   <java_list.txt>
//! codepage verify-single <page> <java_single_page.txt>
//! codepage verify-multi  <page> <java_multi_page.txt> [all]
//! codepage check-tables  <java_rust_tables.txt>
//! codepage gen-fixes     <page> <java_multi_page.txt> [out.rs]
//! codepage dump          <file.mdb> <out.txt>
//! codepage enc|dec       <page> <hex>
//! codepage supported
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::format::Charset;
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "verify-list" => verify_list(&args[2]),
        "verify-single" => verify_single(parse_page(&args[2]), &args[3]),
        "verify-multi" => verify_multi(
            parse_page(&args[2]),
            &args[3],
            args.get(4).map(|s| s == "all").unwrap_or(false),
        ),
        "check-tables" => check_tables(&args[2]),
        "gen-fixes" => gen_fixes(
            parse_page(&args[2]),
            &args[3],
            args.get(4).map(|s| s.as_str()),
        ),
        "enc" => encode_one(&args[2], &args[3]),
        "dec" => decode_one(&args[2], &args[3]),
        "dump" => dump(&args[2], &args[3], args.get(4).map(|p| parse_page(p))),
        "supported" => supported(),
        other => {
            eprintln!("unknown mode {}", other);
            usage();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(2);
    }
}

fn usage() {
    eprintln!("usage: codepage verify-list|verify-single|verify-multi|check-tables|gen-fixes|dump|supported ...");
}

/// The WHATWG encoding of a multi byte page.
fn multi_encoding(page: i16) -> &'static encoding_rs::Encoding {
    match page {
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        _ => panic!("code page {} has no WHATWG encoding", page),
    }
}

/// The decode result of a single two byte unit under the WHATWG encoding, or
/// `None` when the encoding rejects the unit or produces a stateful run.
fn default_pair(
    encoding: &'static encoding_rs::Encoding,
    lead: bool,
    b1: u8,
    b2: u8,
) -> Option<Vec<u16>> {
    if !lead {
        return None;
    }
    let bytes = [b1, b2];
    let (text, had_errors) = encoding.decode_without_bom_handling(&bytes);
    if had_errors {
        return None;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() || units.len() > 2 {
        None
    } else {
        Some(units)
    }
}

/// The WHATWG encoding of one BMP unit, with Java's `?` replacement.
fn default_encode_char(encoding: &'static encoding_rs::Encoding, ch: char) -> Vec<u8> {
    let mut encoder = encoding.new_encoder();
    let mut buf = [0u8; 16];
    let mut utf8 = [0u8; 4];
    let one = ch.encode_utf8(&mut utf8);
    let (result, _read, written) =
        encoder.encode_from_utf8_without_replacement(one, &mut buf, true);
    let mut out = buf[..written].to_vec();
    if matches!(result, encoding_rs::EncoderResult::Unmappable(_)) {
        out.push(b'?');
    }
    out
}

fn gen_fixes(page: i16, path: &str, out: Option<&str>) -> Result<(), String> {
    let lines = read_lines(path)?;
    let mut single = vec![0xFFFDu16; 256];
    let mut pair_expected: Vec<Vec<u16>> = vec![Vec::new(); 65536];
    let mut enc_map: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
    for line in &lines {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("D") => {
                let input = hex_bytes(parts.next().unwrap_or(""))?;
                let output = hex_units(parts.next().unwrap_or(""))?;
                if input.len() == 1 {
                    single[input[0] as usize] = output.first().copied().unwrap_or(0xFFFD);
                } else if input.len() == 2 {
                    pair_expected[((input[0] as usize) << 8) | input[1] as usize] = output;
                }
            }
            Some("e") => {
                let unit = hex_units(parts.next().unwrap_or(""))?;
                let bytes = hex_bytes(parts.next().unwrap_or(""))?;
                if unit.len() == 1 {
                    enc_map.insert(unit[0], bytes);
                }
            }
            _ => {}
        }
    }
    let encoding = multi_encoding(page);
    // A byte is a lead byte exactly when the JDK's b2c array has a row for it.
    // The D lines prove it two ways: a well formed pair, or a pair "lead,0xFF"
    // which the JDK reports as one replacement character (0xFF is neither a
    // lead byte nor a single byte char on these pages, so the pair must have
    // been consumed as one unit).
    if single[0xFF] != 0xFFFD {
        return Err("the 0xFF lead probe needs 0xFF to be unmappable".to_string());
    }
    let mut is_lead = [false; 256];
    for b1 in 0..256usize {
        let pair = &pair_expected[(b1 << 8) | 0xFF];
        if pair.len() == 1 && pair[0] == 0xFFFD {
            is_lead[b1] = true;
        }
    }
    let mut lead_by_pair = [false; 256];
    for b1 in 0..256usize {
        for b2 in 0..256usize {
            let pair = &pair_expected[(b1 << 8) | b2];
            if pair.len() == 1 && pair[0] != 0xFFFD {
                lead_by_pair[b1] = true;
                break;
            }
        }
    }
    for b1 in 0..256usize {
        if is_lead[b1] != lead_by_pair[b1] {
            return Err(format!(
                "lead byte {:02X} disagrees between the 0xFF probe and the pair table",
                b1
            ));
        }
    }
    let mut pair_fix: Vec<(u16, Vec<u16>)> = Vec::new();
    let mut pair_fix_mapped = 0;
    let mut pair_fix_hole = 0;
    for b1 in 0..256u16 {
        for b2 in 0..256u16 {
            let key = (b1 << 8) | b2;
            let pair = &pair_expected[key as usize];
            let expected: Option<Vec<u16>> = if pair.len() == 1 {
                if pair[0] == 0xFFFD {
                    None
                } else {
                    Some(pair.clone())
                }
            } else {
                None
            };
            let default = default_pair(encoding, is_lead[b1 as usize], b1 as u8, b2 as u8);
            if default != expected {
                if expected.is_some() {
                    pair_fix_mapped += 1;
                } else {
                    pair_fix_hole += 1;
                }
                pair_fix.push((key, expected.unwrap_or_default()));
            }
        }
    }
    let mut enc_fix: Vec<(u16, Vec<u8>)> = Vec::new();
    for unit in 0u32..=0xFFFF {
        if (0xD800..=0xDFFF).contains(&unit) {
            continue;
        }
        let ch = match char::from_u32(unit) {
            Some(ch) => ch,
            None => continue,
        };
        let want = enc_map
            .get(&(unit as u16))
            .cloned()
            .unwrap_or_else(|| vec![0x3F]);
        let got = default_encode_char(encoding, ch);
        if got != want {
            enc_fix.push((unit as u16, want));
        }
    }
    let pair_bytes: usize = pair_fix.iter().map(|(_, v)| v.len() * 2).sum();
    let enc_bytes: usize = enc_fix.iter().map(|(_, v)| v.len()).sum();
    println!(
        "page {} {}: lead bytes {}, pair fixes {} ({} mapped, {} holes) {} output units, encode fixes {} {} bytes",
        page,
        Charset::for_code_page(page).map(|c| c.name()).unwrap_or("?"),
        is_lead.iter().filter(|&&b| b).count(),
        pair_fix.len(),
        pair_fix_mapped,
        pair_fix_hole,
        pair_bytes,
        enc_fix.len(),
        enc_bytes
    );
    if let Some(out_path) = out {
        let mut src = String::new();
        src.push_str(&format!(
            "// generated by codepage gen-fixes {} {}; do not edit\n",
            page, path
        ));
        // encode fixes: unit (4 hex) + length (2 hex) + the bytes
        let mut enc_hex = String::new();
        for (unit, bytes) in &enc_fix {
            enc_hex.push_str(&format!("{:04x}{:02x}", unit, bytes.len()));
            for b in bytes {
                enc_hex.push_str(&format!("{:02x}", b));
            }
        }
        // pair fixes: key (4 hex) + replacement unit (4 hex, ffff = a hole)
        let mut pair_hex = String::new();
        for (key, units) in &pair_fix {
            assert!(units.len() <= 1, "pair fix of {} units", units.len());
            pair_hex.push_str(&format!(
                "{:04x}{:04x}",
                key,
                units.first().copied().unwrap_or(0xFFFF)
            ));
        }
        let mut lead_hex = String::new();
        for chunk in is_lead.chunks(4) {
            let mut nibble = 0u8;
            for (bit, lead) in chunk.iter().enumerate() {
                if *lead {
                    nibble |= 1 << (3 - bit);
                }
            }
            lead_hex.push_str(&format!("{:x}", nibble));
        }
        src.push_str(&format!(
            "static MULTI_{}_ENC: [u64; {}] = parse_enc_fix(\"{}\");\n",
            page,
            enc_fix.len(),
            enc_hex
        ));
        if !pair_fix.is_empty() {
            src.push_str(&format!(
                "static MULTI_{}_PAIR: [u32; {}] = parse_pair_fix(\"{}\");\n",
                page,
                pair_fix.len(),
                pair_hex
            ));
        }
        let mut single_hex = String::with_capacity(1024);
        for u in &single {
            single_hex.push_str(&format!("{:04x}", u));
        }
        src.push_str(&format!(
            "const MULTI_{}: MultiTable = MultiTable {{\n    page: {},\n    single: parse_table_hex(\"{}\"),\n    lead_mask: parse_lead_mask(\"{}\"),\n    pair_fix: {},\n    enc_fix: &MULTI_{}_ENC,\n}};\n",
            page,
            page,
            single_hex,
            lead_hex,
            if pair_fix.is_empty() {
                "&[]".to_string()
            } else {
                format!("&MULTI_{}_PAIR", page)
            },
            page
        ));
        std::fs::write(out_path, src).map_err(|e| format!("write {}: {}", out_path, e))?;
        println!("wrote {}", out_path);
    }
    Ok(())
}

fn encode_one(page: &str, units_hex: &str) -> Result<(), String> {
    let cs = Charset::for_code_page(parse_page(page))
        .ok_or_else(|| format!("code page {} is not supported", page))?;
    let units = hex_units(units_hex)?;
    let text = String::from_utf16(&units).map_err(|e| format!("bad text: {}", e))?;
    println!("{}", bytes_hex(&cs.encode(&text)));
    Ok(())
}

fn decode_one(page: &str, bytes_hex: &str) -> Result<(), String> {
    let cs = Charset::for_code_page(parse_page(page))
        .ok_or_else(|| format!("code page {} is not supported", page))?;
    let bytes = hex_bytes(bytes_hex)?;
    println!("{}", units_hex(&cs.decode(&bytes)));
    Ok(())
}

fn parse_page(s: &str) -> i16 {
    let page: u32 = s.parse().expect("page number");
    assert!(page <= 0xFFFF, "page number out of range");
    page as u16 as i16
}

// ---------------------------------------------------------------------------
// text <-> hex helpers
// ---------------------------------------------------------------------------

fn hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {}", s));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char)
            .to_digit(16)
            .ok_or_else(|| format!("bad hex: {}", s))?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("bad hex: {}", s))?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

fn hex_units(s: &str) -> Result<Vec<u16>, String> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(4) {
        return Err(format!("odd unit hex length: {}", s));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4);
    for i in (0..bytes.len()).step_by(4) {
        let mut v: u16 = 0;
        for j in 0..4 {
            let d = (bytes[i + j] as char)
                .to_digit(16)
                .ok_or_else(|| format!("bad hex: {}", s))?;
            v = (v << 4) | d as u16;
        }
        out.push(v);
    }
    Ok(out)
}

fn units_hex(units: &[u16]) -> String {
    let mut s = String::with_capacity(units.len() * 4);
    for u in units {
        s.push_str(&format!("{:04x}", u));
    }
    s
}

fn bytes_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// `Collections.sort` order (UTF-16 code units), which is what the Java dump
/// produces.
fn sort_utf16(lines: &mut [String]) {
    lines.sort_by(|a, b| {
        a.encode_utf16()
            .collect::<Vec<u16>>()
            .cmp(&b.encode_utf16().collect::<Vec<u16>>())
    });
}

fn read_lines(path: &str) -> Result<Vec<String>, String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .map_err(|e| format!("{}: {}", path, e))
}

// ---------------------------------------------------------------------------
// supported
// ---------------------------------------------------------------------------

fn supported() -> Result<(), String> {
    let mut count = 0;
    for page in 1..=i16::MAX as u16 {
        if let Some(cs) = Charset::for_code_page(page as i16) {
            println!("{} {} {:?}", page, cs.name(), cs);
            count += 1;
        }
    }
    println!("supported {}", count);
    Ok(())
}

// ---------------------------------------------------------------------------
// verify-list: every code page the JDK resolves
// ---------------------------------------------------------------------------

fn verify_list(path: &str) -> Result<(), String> {
    let lines = read_lines(path)?;
    let mut java_pages = BTreeMap::new();
    for line in &lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let page: u32 = parts[0].parse().map_err(|_| format!("bad line {}", line))?;
        java_pages.insert(page, (parts[1].to_string(), parts[2].to_string()));
    }
    let mut mapped = 0;
    let mut unsupported_single = Vec::new();
    let mut unsupported_multi = Vec::new();
    let mut name_mismatch = 0;
    for (&page, (name, kind)) in &java_pages {
        // a Jet 3 header stores the code page in a signed short, which is what
        // DatabaseImpl.getCodePageCharset receives; a page above 32767 wraps
        // to a negative short and java returns null for it as well
        let header_value = page as u16 as i16;
        match Charset::for_code_page(header_value) {
            Some(cs) => {
                mapped += 1;
                if cs.name() != name {
                    name_mismatch += 1;
                    println!(
                        "NAME MISMATCH page {}: java {} vs rust {}",
                        page,
                        name,
                        cs.name()
                    );
                }
            }
            None => {
                if header_value > 0 {
                    if kind == "single" {
                        unsupported_single.push((page, name.clone()));
                    } else {
                        unsupported_multi.push((page, name.clone()));
                    }
                }
            }
        }
    }
    // every page the port claims must be one the JDK resolves (otherwise the
    // generated table would be wrong)
    let mut extra = 0;
    for page in 1..=i16::MAX as u16 {
        if Charset::for_code_page(page as i16).is_some() && !java_pages.contains_key(&(page as u32))
        {
            extra += 1;
            println!("EXTRA PAGE page {} not resolved by the jdk", page);
        }
    }
    println!(
        "pages {} ({} single, {} multi); mapped {}; unsupported single {}; unsupported multi {}; name mismatches {}; extra {}",
        java_pages.len(),
        java_pages.values().filter(|(_, k)| k == "single").count(),
        java_pages.values().filter(|(_, k)| k == "multi").count(),
        mapped,
        unsupported_single.len(),
        unsupported_multi.len(),
        name_mismatch,
        extra
    );
    for (page, name) in &unsupported_multi {
        println!("unsupported multi page {} {}", page, name);
    }
    for (page, name) in &unsupported_single {
        println!("UNSUPPORTED single page {} {}", page, name);
    }
    if !unsupported_single.is_empty() || name_mismatch != 0 || extra != 0 {
        return Err("page list does not match the jdk".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify-single: exhaustive single byte page comparison
// ---------------------------------------------------------------------------

fn verify_single(page: i16, path: &str) -> Result<(), String> {
    let lines = read_lines(path)?;
    let mut expected_page = None;
    let mut expected_name = String::new();
    let mut expected_decode: Option<Vec<u16>> = None;
    let mut per_byte: Vec<Option<Vec<u16>>> = vec![None; 256];
    let mut enc: BTreeMap<u16, u8> = BTreeMap::new();
    for line in &lines {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("page") => {
                expected_page = parts.next().and_then(|p| p.parse::<u32>().ok());
            }
            Some("name") => expected_name = parts.next().unwrap_or("").to_string(),
            Some("decode") => expected_decode = Some(hex_units(parts.next().unwrap_or(""))?),
            Some("b") => {
                let b = hex_bytes(parts.next().unwrap_or(""))?;
                let u = hex_units(parts.next().unwrap_or(""))?;
                if b.len() != 1 {
                    return Err(format!("bad b line {}", line));
                }
                per_byte[b[0] as usize] = Some(u);
            }
            Some("enc") => {
                let unit = hex_units(parts.next().unwrap_or(""))?;
                let byte = hex_bytes(parts.next().unwrap_or(""))?;
                if unit.len() != 1 || byte.len() != 1 {
                    return Err(format!("bad enc line {}", line));
                }
                enc.insert(unit[0], byte[0]);
            }
            _ => {}
        }
    }
    if expected_page != Some((page as u16) as u32) {
        return Err(format!(
            "reference file is for page {:?}, expected {}",
            expected_page, page
        ));
    }
    let charset = Charset::for_code_page(page)
        .ok_or_else(|| format!("code page {} is not supported", page))?;
    let mut failures = Vec::new();
    if charset.name() != expected_name {
        failures.push(format!(
            "name: java {} vs rust {}",
            expected_name,
            charset.name()
        ));
    }
    let expected_decode = expected_decode.ok_or("missing decode table")?;
    if expected_decode.len() != 256 {
        return Err("decode table is not 256 units".to_string());
    }
    let all: Vec<u8> = (0..=255u8).collect();
    let got = charset.decode(&all);
    if got != expected_decode {
        let diffs = got
            .iter()
            .zip(expected_decode.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .count();
        failures.push(format!("decode of 256 bytes: {} differing bytes", diffs));
        for (b, (a, e)) in got.iter().zip(expected_decode.iter()).enumerate() {
            if a != e {
                failures.push(format!("  byte {:02x}: java {:04x} rust {:04x}", b, e, a));
            }
        }
    }
    for (b, exp) in per_byte.iter().enumerate() {
        if let Some(exp) = exp {
            let got = charset.decode(&[b as u8]);
            if &got != exp {
                failures.push(format!("per byte {:02x}: java {:?} rust {:?}", b, exp, got));
            }
        }
    }
    // exhaustive encoder comparison: the JDK's bytes for every BMP code unit
    let mut compared = 0;
    let mut mismatch = 0;
    for unit in 0u32..=0xFFFF {
        if (0xD800..=0xDFFF).contains(&unit) {
            continue;
        }
        let ch = match char::from_u32(unit) {
            Some(ch) => ch,
            None => continue,
        };
        let mut buf = [0u8; 4];
        let text = ch.encode_utf8(&mut buf);
        let got = charset.encode(text);
        let want = enc.get(&(unit as u16)).copied().unwrap_or(b'?');
        compared += 1;
        if got.len() != 1 || got[0] != want {
            mismatch += 1;
            if mismatch <= 40 {
                failures.push(format!(
                    "encode U+{:04X}: java {:02x} rust {}",
                    unit,
                    want,
                    bytes_hex(&got)
                ));
            }
        }
    }
    let mut round_trip = 0;
    for (b, unit) in expected_decode.iter().enumerate() {
        if *unit != 0xFFFD
            && charset.encode(&char::from_u32(*unit as u32).unwrap().to_string()) == [b as u8]
        {
            round_trip += 1;
        }
    }
    let mut bad = 0;
    for f in &failures {
        if !f.starts_with("  byte") && !f.starts_with("encode U+") {
            bad += 1;
        }
        println!("{}", f);
    }
    println!(
        "page {} {}: decode {} bytes (table + per byte), encode {} units, round trip bytes {}, failures {}",
        page,
        charset.name(),
        256,
        compared,
        round_trip,
        bad + mismatch
    );
    if mismatch != 0 || bad != 0 {
        return Err(format!("{} single byte mismatches", bad + mismatch));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify-multi: the lead/trail corpus of a multi byte page
// ---------------------------------------------------------------------------

fn verify_multi(page: i16, path: &str, show_all: bool) -> Result<(), String> {
    let lines = read_lines(path)?;
    let mut expected_page = None;
    let mut expected_name = String::new();
    // the bytes the charset writes for a character it cannot encode; every
    // unit absent from the `e` lines must encode to exactly these
    let mut expected_repl: Vec<u8> = vec![0x3F];
    let mut decode_cases: Vec<(Vec<u8>, Vec<u16>)> = Vec::new();
    let mut encode_cases: Vec<(Vec<u16>, Vec<u8>)> = Vec::new();
    let mut full_enc: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
    for line in &lines {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("page") => expected_page = parts.next().and_then(|p| p.parse::<u32>().ok()),
            Some("name") => expected_name = parts.next().unwrap_or("").to_string(),
            Some("repl") => expected_repl = hex_bytes(parts.next().unwrap_or(""))?,
            Some("D") => {
                let input = hex_bytes(parts.next().unwrap_or(""))?;
                let output = hex_units(parts.next().unwrap_or(""))?;
                decode_cases.push((input, output));
            }
            Some("E") => {
                let input = hex_units(parts.next().unwrap_or(""))?;
                let output = hex_bytes(parts.next().unwrap_or(""))?;
                encode_cases.push((input, output));
            }
            Some("e") => {
                let unit = hex_units(parts.next().unwrap_or(""))?;
                let output = hex_bytes(parts.next().unwrap_or(""))?;
                if unit.len() == 1 {
                    full_enc.insert(unit[0], output);
                }
            }
            _ => {}
        }
    }
    if expected_page != Some((page as u16) as u32) {
        return Err(format!(
            "reference file is for page {:?}, expected {}",
            expected_page, page
        ));
    }
    let charset = Charset::for_code_page(page)
        .ok_or_else(|| format!("code page {} is not supported", page))?;
    let limit = if show_all { usize::MAX } else { 30 };
    let mut failures = 0;
    let mut first: Vec<String> = Vec::new();
    let mut decode_len_diffs = 0;
    let mut decode_unit_diffs = 0;
    let mut decode_by_len: BTreeMap<usize, usize> = BTreeMap::new();
    let mut unit_pairs: BTreeMap<u16, BTreeMap<u16, usize>> = BTreeMap::new();
    if charset.name() != expected_name {
        failures += 1;
        first.push(format!(
            "name: java {} vs rust {}",
            expected_name,
            charset.name()
        ));
    }
    for (input, expected) in &decode_cases {
        let got = charset.decode(input);
        if &got == expected {
            continue;
        }
        failures += 1;
        *decode_by_len.entry(input.len()).or_insert(0) += 1;
        if got.len() != expected.len() {
            decode_len_diffs += 1;
        } else {
            decode_unit_diffs += 1;
            for (g, e) in got.iter().zip(expected.iter()) {
                if g != e {
                    *unit_pairs.entry(*g).or_default().entry(*e).or_insert(0) += 1;
                }
            }
        }
        if first.len() < limit {
            first.push(format!(
                "decode {}: java {} rust {}",
                bytes_hex(input),
                units_hex(expected),
                units_hex(&got)
            ));
        }
    }
    let mut encode_failures = 0;
    let mut encode_shown = 0;
    let mut encode_fail_examples: Vec<String> = Vec::new();
    for (input, expected) in &encode_cases {
        let text = match String::from_utf16(input) {
            Ok(t) => t,
            Err(_) => {
                failures += 1;
                encode_failures += 1;
                if first.len() < limit {
                    first.push(format!(
                        "encode {}: lone surrogate in input",
                        units_hex(input)
                    ));
                }
                continue;
            }
        };
        let got = charset.encode(&text);
        if &got != expected {
            failures += 1;
            encode_failures += 1;
            if encode_shown < 20 {
                encode_shown += 1;
                encode_fail_examples.push(format!(
                    "encode {}: java {} rust {}",
                    units_hex(input),
                    bytes_hex(expected),
                    bytes_hex(&got)
                ));
            }
        }
    }
    // the exhaustive encoder map (every BMP unit, Java's '?' when absent)
    let mut full_enc_failures = 0;
    let mut full_enc_compared = 0;
    if !full_enc.is_empty() {
        for unit in 0u32..=0xFFFF {
            if (0xD800..=0xDFFF).contains(&unit) {
                continue;
            }
            let ch = match char::from_u32(unit) {
                Some(ch) => ch,
                None => continue,
            };
            let mut buf = [0u8; 4];
            let text = ch.encode_utf8(&mut buf);
            let got = charset.encode(text);
            let want = full_enc
                .get(&(unit as u16))
                .cloned()
                .unwrap_or_else(|| expected_repl.clone());
            full_enc_compared += 1;
            if got != want {
                failures += 1;
                full_enc_failures += 1;
                if encode_fail_examples.len() < 40 {
                    encode_fail_examples.push(format!(
                        "encode U+{:04X}: java {} rust {}",
                        unit,
                        bytes_hex(&want),
                        bytes_hex(&got)
                    ));
                }
            }
        }
    }
    for f in &first {
        println!("{}", f);
    }
    for f in &encode_fail_examples {
        println!("{}", f);
    }
    println!(
        "page {} {}: decode sequences {}, encode sequences {}, failures {}",
        page,
        charset.name(),
        decode_cases.len(),
        encode_cases.len(),
        failures
    );
    println!(
        "  decode failures by length: {:?}, unit differences {}, length differences {}",
        decode_by_len, decode_unit_diffs, decode_len_diffs
    );
    println!(
        "  encode failures: {} (corpus), {} of {} (exhaustive units)",
        encode_failures, full_enc_failures, full_enc_compared
    );
    if !unit_pairs.is_empty() {
        let ambiguous = unit_pairs.values().filter(|m| m.len() > 1).count();
        println!(
            "  distinct whatwg->java unit pairs {}, ambiguous whatwg units {}",
            unit_pairs.len(),
            ambiguous
        );
        for (g, m) in unit_pairs.iter().take(12) {
            let targets: Vec<String> = m.keys().map(|u| format!("{:04x}", u)).collect();
            println!("    {:04x} -> {}", g, targets.join(","));
        }
    }
    if failures != 0 {
        return Err(format!("{} multi byte mismatches", failures));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// check-tables: the compiled in table must equal the JDK generated source
// ---------------------------------------------------------------------------

fn check_tables(path: &str) -> Result<(), String> {
    let lines = read_lines(path)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut checked = 0;
    let mut failures = 0;
    for line in &lines {
        if !line.contains("CpTable {") {
            continue;
        }
        let page = field_u32(line, "page: ").ok_or("missing page")?;
        let name = field_str(line, "name: \"").ok_or("missing name")?;
        let decode = field_str(line, "parse_table_hex(\"").ok_or("missing decode")?;
        let overrides = field_pairs(line, "extra_enc: &[");
        seen.insert(page);
        let charset = match Charset::for_code_page(page as u16 as i16) {
            Some(cs) => cs,
            None => {
                println!("page {} is in the generated table but not supported", page);
                failures += 1;
                continue;
            }
        };
        if charset.name() != name {
            println!(
                "page {} name: java {} vs rust {}",
                page,
                name,
                charset.name()
            );
            failures += 1;
        }
        let expected = hex_units(&decode)?;
        let all: Vec<u8> = (0..=255u8).collect();
        let got = charset.decode(&all);
        if got != expected {
            let diff = got
                .iter()
                .zip(expected.iter())
                .filter(|(a, b)| a != b)
                .count();
            println!("page {} decode table differs in {} bytes", page, diff);
            failures += 1;
        }
        for (unit, byte) in overrides {
            let ch = match char::from_u32(unit as u32) {
                Some(ch) => ch,
                None => continue,
            };
            let mut buf = [0u8; 4];
            let text = ch.encode_utf8(&mut buf);
            let got = charset.encode(text);
            if got != [byte] {
                println!(
                    "page {} encode U+{:04X}: table {:02x} rust {}",
                    page,
                    unit,
                    byte,
                    bytes_hex(&got)
                );
                failures += 1;
            }
        }
        checked += 1;
    }
    // every supported page must be in the generated table
    for page in 1..=i16::MAX as u16 {
        match Charset::for_code_page(page as i16) {
            Some(Charset::SingleByte(_)) | Some(Charset::Cp1252)
                if !seen.contains(&(page as u32)) =>
            {
                println!(
                    "supported page {} is missing from the generated table",
                    page
                );
                failures += 1;
            }
            _ => {}
        }
    }
    println!(
        "generated table: {} pages, {} overrides checked, failures {}",
        checked,
        seen.len(),
        failures
    );
    if failures != 0 {
        return Err(format!("{} table mismatches", failures));
    }
    Ok(())
}

fn field_u32(line: &str, prefix: &str) -> Option<u32> {
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn field_str(line: &str, prefix: &str) -> Option<String> {
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// pairs of `(0xXXXX, 0xYY)` in the `extra_enc` list
fn field_pairs(line: &str, prefix: &str) -> Vec<(u16, u8)> {
    let mut out = Vec::new();
    let start = match line.find(prefix) {
        Some(s) => s + prefix.len(),
        None => return out,
    };
    let rest = &line[start..];
    let end = rest.find(']').unwrap_or(rest.len());
    let list = &rest[..end];
    for item in list.split("),") {
        let item = item.trim();
        if !item.starts_with('(') {
            continue;
        }
        let item = item.trim_start_matches('(');
        let mut parts = item.split(',');
        let unit = parts
            .next()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        let byte = parts
            .next()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        if let (Some(unit), Some(byte)) = (unit, byte) {
            out.push((unit as u16, byte as u8));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// dump: the text values of a database, in the Java dump format
// ---------------------------------------------------------------------------

/// A table selected for the dump: its name, table-definition page number and
/// its text columns as `(column index, column name)`.
type DumpTable = (String, i32, Vec<(usize, String)>);

/// `dump <file> <out> [readPage]`: the text values of a database, in the Java
/// dump format.  When `read_page` is not `None`, the database charset is
/// switched to that code page after the system catalog was read, mirroring
/// Java's `Database.setCharset` override (the EBCDIC pages cannot be used to
/// read the catalog, which stores ASCII table names).
fn dump(path: &str, out_path: &str, read_page: Option<i16>) -> Result<(), String> {
    let mut db = Database::open_path_read_only(Path::new(path))
        .map_err(|e| format!("open {}: {}", path, e))?;
    // the table and column names come from the catalog, which stores ASCII:
    // collect them before switching the charset (a lookup made after the
    // switch would decode the catalog names with the new page and fail)
    let mut names = db.table_names();
    names.sort();
    let mut tables: Vec<DumpTable> = Vec::new();
    for name in &names {
        if name.to_lowercase().starts_with("msys") {
            continue;
        }
        if let Ok(Some(table)) = db.find_table(name) {
            let cols = table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| matches!(c.data_type, DataType::Text | DataType::Memo))
                .map(|(i, c)| (i, c.name.clone()))
                .collect();
            tables.push((name.clone(), table.table_def_page_number, cols));
        }
    }
    if let Some(read_page) = read_page {
        if read_page != 0 {
            let charset = Charset::for_code_page(read_page)
                .ok_or_else(|| format!("code page {} is not supported", read_page))?;
            db.set_charset(Some(charset));
        }
    }
    let charset_name = db.charset.name().to_string();
    let page = db.default_code_page.unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    for (name, tdef_page, cols) in &tables {
        match db.read_table_rows(*tdef_page) {
            Ok(rows) => {
                for row in rows.into_iter().flatten() {
                    for (index, col) in cols {
                        let line = match row.get(*index) {
                            Some(Value::Text(text)) => {
                                format!(
                                    "text {} {} {}",
                                    name.to_uppercase(),
                                    col.to_uppercase(),
                                    units_hex(&text.encode_utf16().collect::<Vec<u16>>())
                                )
                            }
                            Some(Value::Null) | None => {
                                format!("null {} {}", name.to_uppercase(), col.to_uppercase())
                            }
                            Some(other) => {
                                format!(
                                    "other {} {} {:?}",
                                    name.to_uppercase(),
                                    col.to_uppercase(),
                                    other
                                )
                            }
                        };
                        lines.push(line);
                    }
                }
            }
            Err(e) => eprintln!("read {} failed: {}", name, e),
        }
    }
    sort_utf16(&mut lines);
    let mut out = String::new();
    out.push_str(&format!("charset {}\n", charset_name));
    out.push_str(&format!("page {}\n", page));
    out.push_str(&format!("count {}\n", lines.len()));
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(out_path, out).map_err(|e| format!("write {}: {}", out_path, e))?;
    println!(
        "dump {}: charset {}, page {}, {} text values",
        path,
        charset_name,
        page,
        lines.len()
    );
    Ok(())
}
