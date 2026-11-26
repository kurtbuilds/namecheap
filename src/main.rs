use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const API_URL: &str = "https://api.namecheap.com/xml.response";
const API_ACCESS_URL: &str = "https://ap.www.namecheap.com/settings/tools/apiaccess/";
const IP_URL: &str = "https://api.ipify.org";

#[derive(Parser)]
#[command(name = "namecheap", version, about = "A simple CLI for Namecheap DNS")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save your Namecheap API credentials to ~/.config/namecheap.toml
    Setup,
    /// List all domains in the account
    List,
    /// Show all DNS records for a domain
    Show { domain: String },
    /// Add a DNS record to a domain, e.g. `namecheap add example.com www A 1.2.3.4`
    Add {
        domain: String,
        /// Host name relative to the domain: `www`, `@` for the apex, `*` for wildcard
        name: String,
        /// Record type: A, AAAA, CNAME, MX, TXT, NS, CAA, ALIAS, URL, URL301, FRAME
        #[arg(value_name = "TYPE")]
        record_type: String,
        /// Record value (IP address, target host, text, ...)
        value: String,
        /// Time to live in seconds
        #[arg(long, default_value_t = 1800)]
        ttl: u32,
        /// Priority, only used for MX records
        #[arg(long, default_value_t = 10)]
        mx_pref: u32,
    },
}

#[derive(Serialize, Deserialize)]
struct Config {
    /// Namecheap account username
    username: String,
    api_key: String,
    /// Whitelisted IPv4 address to send as ClientIp. Auto-detected when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_ip: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("could not find home directory"))?;
    Ok(home.join(".config").join("namecheap.toml"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    let text = fs::read_to_string(&path).with_context(|| {
        format!("could not read {} (run `namecheap setup` first)", path.display())
    })?;
    toml::from_str(&text).with_context(|| format!("invalid config in {}", path.display()))
}

fn save_config(config: &Config) -> Result<PathBuf> {
    let path = config_path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options.open(&path)?;
    file.write_all(toml::to_string(config)?.as_bytes())?;
    Ok(path)
}

fn public_ip() -> Result<String> {
    let ip = ureq::get(IP_URL)
        .call()
        .context("could not detect public IP address")?
        .into_string()?;
    Ok(ip.trim().to_string())
}

#[derive(Debug, Clone, PartialEq)]
struct Host {
    name: String,
    record_type: String,
    address: String,
    mx_pref: String,
    ttl: String,
}

struct Zone {
    email_type: String,
    using_namecheap_dns: bool,
    hosts: Vec<Host>,
}

struct Domain {
    name: String,
    expires: String,
    auto_renew: bool,
    namecheap_dns: bool,
}

/// Parse an API response, turning API-level errors into `Err`.
fn parse_response(body: &str) -> Result<roxmltree::Document<'_>> {
    let doc = roxmltree::Document::parse(body).context("could not parse API response")?;
    let errors: Vec<String> = doc
        .descendants()
        .filter(|n| n.has_tag_name("Error"))
        .map(|n| {
            let text = n.text().unwrap_or("unknown error").trim();
            match n.attribute("Number") {
                Some(number) => format!("{text} (error {number})"),
                None => text.to_string(),
            }
        })
        .collect();
    if !errors.is_empty() {
        bail!("Namecheap API error: {}", errors.join("; "));
    }
    if doc.root_element().attribute("Status") != Some("OK") {
        bail!("Namecheap API returned an unexpected response:\n{body}");
    }
    Ok(doc)
}

fn parse_domains(body: &str) -> Result<(Vec<Domain>, usize)> {
    let doc = parse_response(body)?;
    let domains = doc
        .descendants()
        .filter(|n| n.has_tag_name("Domain"))
        .map(|n| Domain {
            name: n.attribute("Name").unwrap_or_default().to_string(),
            expires: n.attribute("Expires").unwrap_or_default().to_string(),
            auto_renew: n.attribute("AutoRenew") == Some("true"),
            namecheap_dns: n.attribute("IsOurDNS") == Some("true"),
        })
        .collect();
    let total = doc
        .descendants()
        .find(|n| n.has_tag_name("TotalItems"))
        .and_then(|n| n.text())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    Ok((domains, total))
}

fn parse_zone(body: &str) -> Result<Zone> {
    let doc = parse_response(body)?;
    let result = doc
        .descendants()
        .find(|n| n.has_tag_name("DomainDNSGetHostsResult"))
        .ok_or_else(|| anyhow!("API response did not include any host data"))?;
    let hosts = result
        .children()
        .filter(|n| n.is_element())
        .map(|n| Host {
            name: n.attribute("Name").unwrap_or_default().to_string(),
            record_type: n.attribute("Type").unwrap_or_default().to_string(),
            address: n.attribute("Address").unwrap_or_default().to_string(),
            mx_pref: n.attribute("MXPref").unwrap_or("10").to_string(),
            ttl: n.attribute("TTL").unwrap_or("1800").to_string(),
        })
        .collect();
    Ok(Zone {
        email_type: result.attribute("EmailType").unwrap_or_default().to_string(),
        using_namecheap_dns: result.attribute("IsUsingOurDNS") == Some("true"),
        hosts,
    })
}

/// Split `example.co.uk` into the SLD (`example`) and TLD (`co.uk`) the API wants.
fn split_domain(domain: &str) -> Result<(&str, &str)> {
    match domain.split_once('.') {
        Some((sld, tld)) if !sld.is_empty() && !tld.is_empty() => Ok((sld, tld)),
        _ => bail!("`{domain}` is not a valid domain name (expected something like example.com)"),
    }
}

struct Client {
    config: Config,
    client_ip: String,
}

impl Client {
    fn new(config: Config) -> Result<Self> {
        let client_ip = match &config.client_ip {
            Some(ip) => ip.clone(),
            None => public_ip()?,
        };
        Ok(Self { config, client_ip })
    }

    fn call(&self, command: &str, params: &[(&str, &str)]) -> Result<String> {
        let mut form = vec![
            ("ApiUser", self.config.username.as_str()),
            ("ApiKey", self.config.api_key.as_str()),
            ("UserName", self.config.username.as_str()),
            ("ClientIp", self.client_ip.as_str()),
            ("Command", command),
        ];
        form.extend_from_slice(params);
        let response = match ureq::post(API_URL).send_form(&form) {
            Ok(response) => response,
            // The API reports most errors in the body, so keep it.
            Err(ureq::Error::Status(_, response)) => response,
            Err(err) => return Err(err).context("request to Namecheap failed"),
        };
        Ok(response.into_string()?)
    }

    fn domains(&self) -> Result<Vec<Domain>> {
        let mut domains = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let body = self.call(
                "namecheap.domains.getList",
                &[("PageSize", "100"), ("Page", &page), ("SortBy", "NAME")],
            )?;
            let (batch, total) = parse_domains(&body)?;
            let done = batch.is_empty();
            domains.extend(batch);
            if done || domains.len() >= total {
                break;
            }
        }
        Ok(domains)
    }

    fn zone(&self, domain: &str) -> Result<Zone> {
        let (sld, tld) = split_domain(domain)?;
        let body = self.call("namecheap.domains.dns.getHosts", &[("SLD", sld), ("TLD", tld)])?;
        parse_zone(&body)
    }

    /// Replace every host record on the domain. The API has no way to add just one.
    fn set_hosts(&self, domain: &str, email_type: &str, hosts: &[Host]) -> Result<()> {
        let (sld, tld) = split_domain(domain)?;
        let mut owned: Vec<(String, &str)> = Vec::new();
        for (i, host) in hosts.iter().enumerate() {
            let n = i + 1;
            owned.push((format!("HostName{n}"), &host.name));
            owned.push((format!("RecordType{n}"), &host.record_type));
            owned.push((format!("Address{n}"), &host.address));
            owned.push((format!("MXPref{n}"), &host.mx_pref));
            owned.push((format!("TTL{n}"), &host.ttl));
        }
        let mut params = vec![("SLD", sld), ("TLD", tld)];
        if !email_type.is_empty() {
            params.push(("EmailType", email_type));
        }
        params.extend(owned.iter().map(|(k, v)| (k.as_str(), *v)));
        let body = self.call("namecheap.domains.dns.setHosts", &params)?;
        let doc = parse_response(&body)?;
        let success = doc
            .descendants()
            .find(|n| n.has_tag_name("DomainDNSSetHostsResult"))
            .and_then(|n| n.attribute("IsSuccess"));
        if success != Some("true") {
            bail!("Namecheap did not confirm the update:\n{body}");
        }
        Ok(())
    }
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let print_row = |cells: Vec<&str>| {
        let line: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    };
    print_row(headers.to_vec());
    for row in rows {
        print_row(row.iter().map(String::as_str).collect());
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn setup() -> Result<()> {
    println!("Namecheap API access is managed at:\n\n  {API_ACCESS_URL}\n");
    println!("(Profile > Tools > Business & Dev Tools > Namecheap API Access)\n");
    println!("On that page:");
    println!("  1. Turn API access ON if it is not already.");
    println!("  2. Add this machine's public IPv4 address to the Whitelisted IPs.");
    println!("  3. Copy the API key.\n");
    match public_ip() {
        Ok(ip) => println!("Your public IP address appears to be: {ip}\n"),
        Err(_) => println!("(Could not detect your public IP address automatically.)\n"),
    }
    if open::that(API_ACCESS_URL).is_ok() {
        println!("Opening that page in your browser...\n");
    }

    let username = prompt("Namecheap username: ")?;
    if username.is_empty() {
        bail!("username is required");
    }
    let api_key = rpassword::prompt_password("API key (input hidden): ")?
        .trim()
        .to_string();
    if api_key.is_empty() {
        bail!("API key is required");
    }

    // Keep a manually configured client_ip if one was already set.
    let client_ip = load_config().ok().and_then(|c| c.client_ip);
    let config = Config { username, api_key, client_ip };
    let path = save_config(&config)?;
    println!("\nSaved credentials to {}", path.display());

    match Client::new(config).and_then(|client| client.domains()) {
        Ok(domains) => println!("Verified: the account has {} domain(s).", domains.len()),
        Err(err) => {
            println!("\nThe credentials were saved, but a test request failed:\n  {err:#}");
            println!("Check the username, key, and IP whitelist, then try `namecheap list`.");
        }
    }
    Ok(())
}

fn list() -> Result<()> {
    let domains = Client::new(load_config()?)?.domains()?;
    if domains.is_empty() {
        println!("No domains found.");
        return Ok(());
    }
    let yes_no = |b: bool| if b { "yes" } else { "no" }.to_string();
    let rows: Vec<Vec<String>> = domains
        .into_iter()
        .map(|d| vec![d.name, d.expires, yes_no(d.auto_renew), yes_no(d.namecheap_dns)])
        .collect();
    print_table(&["DOMAIN", "EXPIRES", "AUTO-RENEW", "NAMECHEAP DNS"], &rows);
    Ok(())
}

fn show(domain: &str) -> Result<()> {
    let zone = Client::new(load_config()?)?.zone(domain)?;
    if !zone.using_namecheap_dns {
        println!("Note: {domain} does not use Namecheap's DNS, so these records are not live.\n");
    }
    if zone.hosts.is_empty() {
        println!("No records found for {domain}.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = zone
        .hosts
        .into_iter()
        .map(|h| {
            let mx_pref = if h.record_type == "MX" { h.mx_pref } else { String::new() };
            vec![h.name, h.record_type, h.address, h.ttl, mx_pref]
        })
        .collect();
    print_table(&["NAME", "TYPE", "VALUE", "TTL", "PRIORITY"], &rows);
    Ok(())
}

fn add(domain: &str, new: Host) -> Result<()> {
    let client = Client::new(load_config()?)?;
    let zone = client.zone(domain)?;
    if !zone.using_namecheap_dns {
        bail!("{domain} does not use Namecheap's DNS, so records cannot be added here");
    }
    let duplicate = zone.hosts.iter().any(|h| {
        h.name.eq_ignore_ascii_case(&new.name)
            && h.record_type == new.record_type
            && h.address.trim_end_matches('.') == new.address.trim_end_matches('.')
    });
    if duplicate {
        bail!("{domain} already has that record");
    }

    // Custom MX records are only served when the domain's mail setting is "MX".
    let email_type = if new.record_type == "MX" { "MX" } else { zone.email_type.as_str() };
    let mut hosts = zone.hosts.clone();
    hosts.push(new.clone());
    client.set_hosts(domain, email_type, &hosts)?;
    println!(
        "Added {} record: {} -> {} (TTL {})",
        new.record_type, new.name, new.address, new.ttl
    );
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Setup => setup(),
        Command::List => list(),
        Command::Show { domain } => show(&domain),
        Command::Add { domain, name, record_type, value, ttl, mx_pref } => add(
            &domain,
            Host {
                name,
                record_type: record_type.to_ascii_uppercase(),
                address: value,
                mx_pref: mx_pref.to_string(),
                ttl: ttl.to_string(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_domains() {
        assert_eq!(split_domain("example.com").unwrap(), ("example", "com"));
        assert_eq!(split_domain("example.co.uk").unwrap(), ("example", "co.uk"));
        assert!(split_domain("example").is_err());
    }

    #[test]
    fn parses_domain_list() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<ApiResponse Status="OK" xmlns="http://api.namecheap.com/xml.response">
  <Errors />
  <CommandResponse Type="namecheap.domains.getList">
    <DomainGetListResult>
      <Domain ID="1" Name="example.com" User="me" Created="02/15/2016" Expires="02/15/2027" IsExpired="false" IsLocked="false" AutoRenew="true" WhoisGuard="ENABLED" IsPremium="false" IsOurDNS="true" />
    </DomainGetListResult>
    <Paging><TotalItems>1</TotalItems><CurrentPage>1</CurrentPage><PageSize>100</PageSize></Paging>
  </CommandResponse>
</ApiResponse>"#;
        let (domains, total) = parse_domains(body).unwrap();
        assert_eq!(total, 1);
        assert_eq!(domains[0].name, "example.com");
        assert!(domains[0].auto_renew && domains[0].namecheap_dns);
    }

    #[test]
    fn parses_hosts() {
        let body = r#"<ApiResponse Status="OK" xmlns="http://api.namecheap.com/xml.response">
  <Errors />
  <CommandResponse Type="namecheap.domains.dns.getHosts">
    <DomainDNSGetHostsResult Domain="example.com" EmailType="FWD" IsUsingOurDNS="true">
      <host HostId="12" Name="@" Type="A" Address="1.2.3.4" MXPref="10" TTL="1800" />
      <host HostId="14" Name="www" Type="CNAME" Address="example.com." MXPref="10" TTL="300" />
    </DomainDNSGetHostsResult>
  </CommandResponse>
</ApiResponse>"#;
        let zone = parse_zone(body).unwrap();
        assert_eq!(zone.email_type, "FWD");
        assert!(zone.using_namecheap_dns);
        assert_eq!(zone.hosts.len(), 2);
        assert_eq!(zone.hosts[1].record_type, "CNAME");
        assert_eq!(zone.hosts[1].ttl, "300");
    }

    #[test]
    fn surfaces_api_errors() {
        let body = r#"<ApiResponse Status="ERROR" xmlns="http://api.namecheap.com/xml.response">
  <Errors><Error Number="1011150">Invalid request IP: 1.2.3.4</Error></Errors>
</ApiResponse>"#;
        let err = parse_response(body).unwrap_err().to_string();
        assert!(err.contains("Invalid request IP") && err.contains("1011150"));
    }
}
