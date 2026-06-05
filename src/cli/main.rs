use std::{fs, path::PathBuf, process::Command, time::Duration};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ZERO_ADDR: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Serialize, Deserialize, Clone)]
struct Profile {
    provider: String,
    network: String,
    explorer: String,
    auto_anchor: bool,
    #[serde(default)]
    address: String,
    #[serde(default)]
    private_key: String,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("suidrop")
        .join("config.toml")
}

fn load_profile() -> Option<Profile> {
    let s = fs::read_to_string(config_path()).ok()?;
    toml::from_str(&s).ok()
}

fn save_profile(p: &Profile) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, toml::to_string_pretty(p)?)?;
    Ok(())
}

fn banner() {
    let art = r#"
   ____         _   ____
  / ___|  _   _(_) |  _ \ _ __ ___  _ __
  \___ \ | | | | | | | | | '__/ _ \| '_ \
   ___) || |_| | | | |_| | | | (_) | |_) |
  |____/  \__,_|_| |____/|_|  \___/| .__/
                                   |_|
"#;
    println!("{}", style(art).cyan().bold());
    println!(
        "  {}   {}\n",
        style("trustless encrypted file transfer on Sui + Walrus").blue(),
        style(format!("v{}", env!("CARGO_PKG_VERSION"))).dim()
    );
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

fn api(provider: &str, path: &str) -> String {
    format!(
        "{}/{}",
        provider.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn web_base(provider: &str) -> String {
    let p = provider.trim_end_matches('/');
    let p = p.strip_suffix("/api").unwrap_or(p);
    p.replace("://api.", "://")
}

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(90));
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
    );
    pb.set_message(msg.to_string());
    pb
}

fn normalize_network(n: &str) -> String {
    n.trim().trim_start_matches("sui-").to_string()
}

fn extract_blob_id(v: &Value) -> Option<String> {
    v["newlyCreated"]["blobObject"]["blobId"]
        .as_str()
        .or_else(|| v["alreadyCertified"]["blobId"].as_str())
        .or_else(|| v["blobId"].as_str())
        .map(|s| s.to_string())
}

fn encrypt(name: &str, file_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    let meta =
        json!({ "name": name, "type": "application/octet-stream", "size": file_bytes.len() });
    let meta_bytes = serde_json::to_vec(&meta)?;
    let mut plain = Vec::with_capacity(4 + meta_bytes.len() + file_bytes.len());
    plain.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    plain.extend_from_slice(&meta_bytes);
    plain.extend_from_slice(file_bytes);

    let kb: [u8; 32] = rand::random();
    let nb: [u8; 12] = rand::random();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&kb));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nb), plain.as_ref())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    let mut blob = Vec::with_capacity(12 + ct.len());
    blob.extend_from_slice(&nb);
    blob.extend_from_slice(&ct);
    Ok((blob, URL_SAFE_NO_PAD.encode(kb)))
}

fn decrypt(blob: &[u8], key_b64: &str) -> anyhow::Result<(String, Vec<u8>)> {
    if blob.len() < 12 {
        anyhow::bail!("blob too short");
    }
    let kb = URL_SAFE_NO_PAD
        .decode(key_b64)
        .map_err(|_| anyhow::anyhow!("bad key"))?;
    if kb.len() != 32 {
        anyhow::bail!("bad key length");
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&kb));
    let pt = cipher
        .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong key or corrupt blob)"))?;
    if pt.len() < 4 {
        anyhow::bail!("plaintext too short");
    }
    let meta_len = u32::from_be_bytes([pt[0], pt[1], pt[2], pt[3]]) as usize;
    if pt.len() < 4 + meta_len {
        anyhow::bail!("bad envelope");
    }
    let meta: Value = serde_json::from_slice(&pt[4..4 + meta_len])?;
    let name = meta["name"].as_str().unwrap_or("download").to_string();
    Ok((name, pt[4 + meta_len..].to_vec()))
}

fn fetch_config(provider: &str) -> Option<Value> {
    http().get(api(provider, "config")).send().ok()?.json().ok()
}

fn fetch_official_network(provider: &str) -> Option<String> {
    let v: Value = http()
        .get(api(provider, "official-network"))
        .send()
        .ok()?
        .json()
        .ok()?;
    v["network"].as_str().map(normalize_network)
}

fn sui_available() -> bool {
    Command::new("sui")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn export_key(address: &str) -> Option<String> {
    let out = Command::new("sui")
        .args(["keytool", "export", "--key-identity", address, "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["exportedPrivateKey"].as_str().map(|s| s.to_string())
}

fn keygen() -> anyhow::Result<(String, String)> {
    if !sui_available() {
        anyhow::bail!("the `sui` CLI is not installed");
    }
    let out = Command::new("sui")
        .args(["client", "new-address", "ed25519", "--json"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let address = v["address"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no address in output"))?
        .to_string();
    let _ = Command::new("sui")
        .args(["client", "switch", "--address", &address])
        .output();
    let pk = export_key(&address).unwrap_or_default();
    Ok((address, pk))
}

fn keyimport(key: &str) -> anyhow::Result<String> {
    if !sui_available() {
        anyhow::bail!("the `sui` CLI is not installed");
    }
    let out = Command::new("sui")
        .args(["keytool", "import", key, "ed25519", "--json"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let address = v["suiAddress"]
        .as_str()
        .or_else(|| v["address"].as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no address in import output"))?
        .to_string();
    let _ = Command::new("sui")
        .args(["client", "switch", "--address", &address])
        .output();
    Ok(address)
}

fn balance(profile: &Profile) -> Option<u128> {
    let body = json!({ "id": 1, "jsonrpc": "2.0", "method": "suix_getBalance", "params": [profile.address] });
    let v: Value = http()
        .post(api(&profile.provider, "rpc"))
        .json(&body)
        .send()
        .ok()?
        .json()
        .ok()?;
    v["result"]["totalBalance"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
}

fn request_faucet(address: &str) -> bool {
    let body = json!({ "FixedAmountRequest": { "recipient": address } });
    http()
        .post("https://faucet.testnet.sui.io/v1/gas")
        .json(&body)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn maybe_fund(profile: &Profile) {
    if profile.address.is_empty() {
        println!("  {}", style("No address configured.").yellow());
        return;
    }
    let pb = spinner("Checking balance...");
    let bal = balance(profile);
    pb.finish_and_clear();
    if let Some(b) = bal {
        if b > 0 {
            println!("  {} {} MIST", style("Balance:").green(), b);
            return;
        }
    }
    println!(
        "  {}",
        style("Balance is 0. Requesting testnet funds...").yellow()
    );
    let pb = spinner("Asking the faucet...");
    let ok = request_faucet(&profile.address);
    pb.finish_and_clear();
    if ok {
        println!(
            "  {}",
            style("Faucet request sent. Funds should arrive shortly.").green()
        );
    } else {
        println!(
            "  {}",
            style("Faucet request failed. Fund this address yourself:").red()
        );
        println!("    {}", style(&profile.address).cyan());
        println!("    {}", style("https://faucet.sui.io").dim());
        if profile.auto_anchor {
            println!(
                "  {}",
                style("Heads up: auto-anchoring will fail until the wallet has gas.").yellow()
            );
        }
    }
}

fn anchor(
    profile: &Profile,
    blob_id: &str,
    size: usize,
    name: &str,
) -> anyhow::Result<(String, Option<String>)> {
    if !sui_available() {
        anyhow::bail!("`sui` CLI not installed");
    }
    let cfg =
        fetch_config(&profile.provider).ok_or_else(|| anyhow::anyhow!("cannot reach provider"))?;
    let pkg = cfg["packageId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("provider has no package id"))?;
    let epochs = cfg["epochs"].as_u64().unwrap_or(5);

    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let nh = hasher.finalize();
    let nh_arr = format!(
        "[{}]",
        nh.iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );

    let size_s = size.to_string();
    let epochs_s = epochs.to_string();
    let args = [
        "client",
        "call",
        "--package",
        pkg,
        "--module",
        "receipt",
        "--function",
        "create_receipt",
        "--args",
        blob_id,
        ZERO_ADDR,
        size_s.as_str(),
        nh_arr.as_str(),
        epochs_s.as_str(),
        "0x6",
        "--gas-budget",
        "100000000",
        "--json",
    ];
    let out = Command::new("sui").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let digest = v["digest"].as_str().unwrap_or("").to_string();
    let receipt_id = v["objectChanges"].as_array().and_then(|arr| {
        arr.iter()
            .find(|c| {
                c["type"] == "created"
                    && c["objectType"]
                        .as_str()
                        .map(|t| t.contains("::receipt::DropReceipt"))
                        .unwrap_or(false)
            })
            .and_then(|c| c["objectId"].as_str())
            .map(|s| s.to_string())
    });
    Ok((digest, receipt_id))
}

fn cmd_send(profile: &Profile, path: &str) -> anyhow::Result<()> {
    let file_bytes = fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();

    let pb = spinner("Encrypting locally...");
    let (blob, key) = encrypt(&name, &file_bytes)?;
    pb.finish_and_clear();

    let pb = spinner("Uploading to Walrus...");
    let resp = http()
        .post(api(&profile.provider, "walrus/upload"))
        .header("content-type", "application/octet-stream")
        .body(blob.clone())
        .send()?;
    pb.finish_and_clear();
    if !resp.status().is_success() {
        anyhow::bail!("walrus upload failed ({})", resp.status());
    }
    let blob_id =
        extract_blob_id(&resp.json()?).ok_or_else(|| anyhow::anyhow!("no blob id returned"))?;
    println!(
        "  {} {}",
        style("Stored on Walrus:").green(),
        style(&blob_id).cyan()
    );

    let mut receipt_id: Option<String> = None;
    if profile.auto_anchor && !profile.address.is_empty() {
        let pb = spinner("Anchoring on Sui...");
        match anchor(profile, &blob_id, blob.len(), &name) {
            Ok((digest, rid)) => {
                pb.finish_and_clear();
                receipt_id = rid;
                println!(
                    "  {} {}",
                    style("Anchored on Sui:").green(),
                    style(&digest).cyan()
                );
            }
            Err(e) => {
                pb.finish_and_clear();
                println!("  {} {}", style("Anchoring skipped:").yellow(), e);
            }
        }
    }

    let mut link = format!("{}/app?d={}", web_base(&profile.provider), blob_id);
    if let Some(r) = &receipt_id {
        link.push_str(&format!("&r={r}"));
    }
    link.push('#');
    link.push_str(&key);

    println!("\n  {}", style("Share link:").bold());
    println!("  {}\n", style(&link).cyan());
    Ok(())
}

fn cmd_get(profile: &Profile, link: &str, out: Option<&str>) -> anyhow::Result<()> {
    let (no_frag, key) = link.split_once('#').unwrap_or((link, ""));
    if key.is_empty() {
        anyhow::bail!("link has no key (the #... part)");
    }
    let mut url = reqwest::Url::parse(no_frag).map_err(|_| anyhow::anyhow!("invalid link"))?;
    if url.path().starts_with("/s/") {
        let pb = spinner("Resolving short link...");
        let resp = http().get(url.clone()).send()?;
        pb.finish_and_clear();
        url = resp.url().clone();
    }
    let blob_id = url
        .query_pairs()
        .find(|(k, _)| k.as_ref() == "d")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| anyhow::anyhow!("link has no blob id"))?;

    let pb = spinner("Fetching from Walrus...");
    let resp = http()
        .get(api(&profile.provider, &format!("walrus/blob/{blob_id}")))
        .send()?;
    pb.finish_and_clear();
    if !resp.status().is_success() {
        anyhow::bail!("could not fetch blob ({})", resp.status());
    }
    let data = resp.bytes()?.to_vec();

    let pb = spinner("Decrypting locally...");
    let (name, file) = decrypt(&data, key)?;
    pb.finish_and_clear();

    let out_path = out.map(|s| s.to_string()).unwrap_or(name);
    fs::write(&out_path, &file)?;
    println!("  {} {}", style("Saved").green(), style(&out_path).cyan());
    Ok(())
}

fn run_setup() -> anyhow::Result<Profile> {
    let term = console::Term::stdout();
    let _ = term.clear_screen();
    banner();
    println!("  {}\n", style("First-time setup").bold());
    let theme = ColorfulTheme::default();

    let provider: String = Input::<String>::with_theme(&theme)
        .with_prompt("Provider API base")
        .default("https://suidrop.xyz/api".to_string())
        .interact_text()?;

    let suggested = fetch_official_network(&provider).unwrap_or_else(|| "testnet".to_string());
    let network: String = Input::<String>::with_theme(&theme)
        .with_prompt("Network")
        .default(suggested)
        .interact_text()?;
    let network = normalize_network(&network);

    let explorers = ["suiscan", "suivision"];
    let ex = Select::with_theme(&theme)
        .with_prompt("Explorer")
        .items(&explorers)
        .default(0)
        .interact()?;
    let explorer = explorers[ex].to_string();

    let mut auto_anchor = Confirm::with_theme(&theme)
        .with_prompt("Auto-anchor drops on Sui? (recommended)")
        .default(true)
        .interact()?;

    let mut address = String::new();
    let mut private_key = String::new();
    let want_key = Confirm::with_theme(&theme)
        .with_prompt("Set up a signing key now? (recommended for anchoring)")
        .default(true)
        .interact()?;
    if want_key {
        let opts = ["Auto-generate a new key", "Paste an existing private key"];
        let choice = Select::with_theme(&theme)
            .with_prompt("Key")
            .items(&opts)
            .default(0)
            .interact()?;
        if choice == 0 {
            match keygen() {
                Ok((a, k)) => {
                    address = a;
                    private_key = k;
                    println!(
                        "  {} {}",
                        style("New address:").green(),
                        style(&address).cyan()
                    );
                }
                Err(e) => println!("  {} {}", style("Could not generate key:").yellow(), e),
            }
        } else {
            let key: String = Input::<String>::with_theme(&theme)
                .with_prompt("Private key (suiprivkey...)")
                .interact_text()?;
            match keyimport(&key) {
                Ok(a) => {
                    address = a;
                    private_key = key;
                    println!(
                        "  {} {}",
                        style("Imported address:").green(),
                        style(&address).cyan()
                    );
                }
                Err(e) => println!("  {} {}", style("Could not import key:").yellow(), e),
            }
        }
    }

    if auto_anchor && address.is_empty() {
        println!(
            "  {}",
            style("No signing key set, so auto-anchoring is disabled.").yellow()
        );
        auto_anchor = false;
    }

    let profile = Profile {
        provider,
        network,
        explorer,
        auto_anchor,
        address,
        private_key,
    };
    save_profile(&profile)?;
    println!(
        "\n  {} {}",
        style("Saved profile to").dim(),
        style(config_path().display()).dim()
    );

    if profile.network == "testnet" && !profile.address.is_empty() {
        maybe_fund(&profile);
    }
    Ok(profile)
}

fn pause() {
    println!("\n  {}", style("Press Enter to continue...").dim());
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

fn menu(mut profile: Profile) -> anyhow::Result<()> {
    let theme = ColorfulTheme::default();
    loop {
        let term = console::Term::stdout();
        let _ = term.clear_screen();
        banner();
        println!(
            "  {} {}    {} {}\n",
            style("network:").dim(),
            style(&profile.network).cyan(),
            style("anchor:").dim(),
            style(if profile.auto_anchor { "on" } else { "off" }).cyan()
        );

        let items = [
            "Send a file",
            "Receive a file",
            "Settings",
            "Fund wallet",
            "Exit",
        ];
        let choice = Select::with_theme(&theme)
            .items(&items)
            .default(0)
            .interact()?;
        match choice {
            0 => {
                let path: String = Input::<String>::with_theme(&theme)
                    .with_prompt("File path")
                    .interact_text()?;
                if let Err(e) = cmd_send(&profile, &path) {
                    println!("  {} {}", style("Error:").red(), e);
                }
                pause();
            }
            1 => {
                let link: String = Input::<String>::with_theme(&theme)
                    .with_prompt("Share link")
                    .interact_text()?;
                if let Err(e) = cmd_get(&profile, &link, None) {
                    println!("  {} {}", style("Error:").red(), e);
                }
                pause();
            }
            2 => {
                profile = run_setup()?;
                pause();
            }
            3 => {
                maybe_fund(&profile);
                pause();
            }
            _ => break,
        }
    }
    Ok(())
}

fn with_profile<F: FnOnce(Profile) -> anyhow::Result<()>>(f: F) -> anyhow::Result<()> {
    let profile = match load_profile() {
        Some(p) => p,
        None => run_setup()?,
    };
    f(profile)
}

fn print_help() {
    println!("suidrop-cli {}", env!("CARGO_PKG_VERSION"));
    println!("trustless encrypted file transfer on Sui + Walrus");
    println!();
    println!("Usage: suidrop-cli [command]");
    println!();
    println!("Commands:");
    println!("  (no command)   open the interactive menu");
    println!("  setup          configure provider, network, and signing key");
    println!("  send <file>    encrypt, store on Walrus, optionally anchor, print a link");
    println!("  get <link>     verify, fetch, decrypt, save");
    println!("  fund           request testnet gas for the configured address");
    println!("  version        print the version");
    println!("  help           print this help");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(|s| s.as_str()) {
        Some("version") | Some("--version") | Some("-v") => {
            println!("suidrop-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("setup") => run_setup().map(|_| ()),
        Some("send") => with_profile(|p| {
            let path = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: suidrop-cli send <file>"))?;
            cmd_send(&p, path)
        }),
        Some("get") => with_profile(|p| {
            let link = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: suidrop-cli get <link> [output]"))?;
            cmd_get(&p, link, args.get(3).map(|s| s.as_str()))
        }),
        Some("fund") => with_profile(|p| {
            maybe_fund(&p);
            Ok(())
        }),
        _ => with_profile(menu),
    };
    if let Err(e) = result {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
}
