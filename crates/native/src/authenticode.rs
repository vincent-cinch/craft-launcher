//! Windows: who signed an installer (Authenticode), checked before it runs.
//!
//! The macOS install pins Apple's Team ID (`install::macos::TEAM_ID`); this is the Windows
//! counterpart. Every Craft App MSI must carry a signature that Windows calls Valid, from
//! [`PUBLISHER_CN`] / [`PUBLISHER_O`], issued by one of [`ALLOWED_ISSUERS`], whose chain runs
//! through [`PCA`] to [`ROOT`]. Anything else is refused before `msiexec` starts.
//!
//! Windows does the cryptography: `Get-AuthenticodeSignature` (WinVerifyTrust) checks the file's
//! hash, the signature and its timestamp, so a signature made while its short-lived certificate
//! was valid stays Valid after the certificate expires. The launcher then compares names only.
//! The chain whose names it compares is built with time checks switched off and is never used to
//! judge validity, so nothing here compares a certificate's expiry with the current time.
//!
//! The rules are plain functions over the names, so they are tested on every OS.

/// The publisher that signs every Craft App installer for Windows: Authenticode's signer
/// subject, as found on all 72 Windows files of the mirrored releases (October 2026). Note it is
/// "Inc", while the MSI's own `Publisher` value (see `detect.rs`) says "LLC".
pub const PUBLISHER_CN: &str = "Learning Machines Inc";
pub const PUBLISHER_O: &str = "Learning Machines Inc";

/// The certificate authorities allowed to issue the publisher's signing certificate. Azure
/// Trusted Signing rotates between several, so a new ArtCraft release may come from one not
/// listed here, and installing it is refused with the issuer's name until it's added.
///
/// To add one: after mirroring the release, check each of its Windows files (for example
/// `osslsigncode verify -in <file>` with Microsoft's root CA available). Add the issuer's CN here
/// only if the signer is [`PUBLISHER_CN`], the signature is valid and timestamped, and the chain
/// runs issuer → [`PCA`] → [`ROOT`].
pub const ALLOWED_ISSUERS: &[&str] = &["Microsoft ID Verified CS AOC CA 04"];

/// The intermediate every allowed issuer must chain through.
pub const PCA: &str = "Microsoft ID Verified Code Signing PCA 2021";
/// The root the chain must end at.
pub const ROOT: &str = "Microsoft Identity Verification Root Certificate Authority 2020";
/// The organisation every certificate authority in the chain belongs to.
const MICROSOFT: &str = "Microsoft Corporation";

/// What Windows says about a file's signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    /// `Get-AuthenticodeSignature`'s `Status`: `Valid`, `NotSigned`, `HashMismatch`, …
    pub status: String,
    pub message: String,
    /// The signing certificate's subject and issuer, as .NET writes them (`CN=…, O=…`).
    pub subject: Option<String>,
    pub issuer: Option<String>,
    /// The chain's subjects, signing certificate first, root last.
    pub chain: Vec<String>,
}

/// Reads the JSON the PowerShell check prints.
pub fn parse(json: &str) -> Result<Signature, String> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|_| "Windows gave an unreadable answer about the signature".to_string())?;
    let text = |key: &str| v.get(key).and_then(serde_json::Value::as_str).map(str::to_string);
    let chain = match v.get("chain") {
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
        Some(serde_json::Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    Ok(Signature {
        status: text("status").unwrap_or_default(),
        message: text("message").unwrap_or_default(),
        subject: text("subject"),
        issuer: text("issuer"),
        chain,
    })
}

/// Whether `sig` is the Craft Apps publisher's, by the rules at the top of this module. The error
/// says why not, for the user.
pub fn check(sig: &Signature) -> Result<(), String> {
    match sig.status.as_str() {
        "Valid" => {}
        "NotSigned" => return Err("the installer isn't signed, so it wasn't installed".into()),
        other => {
            let why = if sig.message.trim().is_empty() { other.to_string() } else { format!("{other}: {}", sig.message.trim()) };
            return Err(format!("Windows says the installer's signature isn't valid ({why}), so it wasn't installed"));
        }
    }
    let subject = sig.subject.as_deref().unwrap_or_default();
    if !is_name(subject, PUBLISHER_CN, PUBLISHER_O) {
        return Err(format!("the installer is signed by \"{subject}\", not by {PUBLISHER_CN}, so it wasn't installed"));
    }
    let issuer = sig.issuer.as_deref().unwrap_or_default();
    if !ALLOWED_ISSUERS.iter().any(|cn| is_name(issuer, cn, MICROSOFT)) {
        return Err(format!(
            "the installer's certificate comes from an unknown issuer (\"{issuer}\"), so it wasn't installed. \
             If this is a new ArtCraft release, check its signatures and add the issuer to authenticode.rs"
        ));
    }
    let leads_to_root = matches!(sig.chain.as_slice(), [leaf, ca, pca, root]
        if leaf == subject && ca == issuer && is_name(pca, PCA, MICROSOFT) && is_name(root, ROOT, MICROSOFT));
    if !leads_to_root {
        return Err(format!(
            "the installer's certificate chain ({}) doesn't run through {PCA} to {ROOT}, so it wasn't installed",
            if sig.chain.is_empty() { "empty".to_string() } else { sig.chain.join(" > ") }
        ));
    }
    Ok(())
}

/// Whether the distinguished name `dn` has exactly one CN, equal to `cn`, and exactly one O,
/// equal to `o`.
fn is_name(dn: &str, cn: &str, o: &str) -> bool {
    let parts = rdns(dn);
    let only = |key: &str| {
        let mut values = parts.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
        match (values.next(), values.next()) {
            (Some(v), None) => Some(v),
            _ => None,
        }
    };
    only("CN") == Some(cn) && only("O") == Some(o)
}

/// Splits a distinguished name as .NET writes it (`CN=A, O="B, Inc", C=US`) into its parts.
/// Values with commas come in double quotes, a quote inside them doubled.
fn rdns(dn: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = dn.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts.into_iter().filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect()
}

/// The check, as PowerShell runs it. The file comes in through [`FILE_VAR`], never through the
/// command text, so no file name can change what runs. No double quotes, which Windows would
/// mangle on the way to `-Command`.
pub const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
$s = Get-AuthenticodeSignature -LiteralPath $env:ARTCRAFT_LAUNCHER_SIGNED_FILE; \
$o = [ordered]@{ status = $s.Status.ToString(); message = [string]$s.StatusMessage; subject = $null; issuer = $null; chain = @() }; \
$c = $s.SignerCertificate; \
if ($c) { \
$o.subject = $c.Subject; $o.issuer = $c.Issuer; \
$x = New-Object System.Security.Cryptography.X509Certificates.X509Chain; \
$x.ChainPolicy.RevocationMode = 'NoCheck'; \
$x.ChainPolicy.VerificationFlags = 'IgnoreNotTimeValid, IgnoreCtlNotTimeValid'; \
[void]$x.Build($c); \
$o.chain = @($x.ChainElements | ForEach-Object { $_.Certificate.Subject }) }; \
ConvertTo-Json -InputObject $o -Compress";

/// The environment variable [`SCRIPT`] reads the file's path from.
pub const FILE_VAR: &str = "ARTCRAFT_LAUNCHER_SIGNED_FILE";

/// Checks `file` with Windows' own Authenticode verification, then the publisher rules.
#[cfg(windows)]
pub fn verify(file: &std::path::Path) -> Result<(), String> {
    let root = std::env::var_os("SystemRoot").map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from);
    let powershell = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let out = crate::process::run_env(
        &powershell.to_string_lossy(),
        &["-NoProfile", "-NonInteractive", "-Command", SCRIPT],
        &[(FILE_VAR, file.as_os_str())],
        std::time::Duration::from_secs(120),
    )
    .map_err(|e| format!("couldn't check who signed the installer ({e}), so it wasn't installed"))?;
    let sig = parse(&out)?;
    log::info!("{}: signature {} by {:?}, issued by {:?}", file.display(), sig.status, sig.subject, sig.issuer);
    check(&sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBJECT: &str = "CN=Learning Machines Inc, O=Learning Machines Inc, L=Atlanta, S=Georgia, C=US";
    const ISSUER: &str = "CN=Microsoft ID Verified CS AOC CA 04, O=Microsoft Corporation, C=US";
    const PCA_DN: &str = "CN=Microsoft ID Verified Code Signing PCA 2021, O=Microsoft Corporation, C=US";
    const ROOT_DN: &str = "CN=Microsoft Identity Verification Root Certificate Authority 2020, O=Microsoft Corporation, C=US";

    /// What the mirrored releases' MSIs carry (see the module docs).
    fn real() -> Signature {
        Signature {
            status: "Valid".into(),
            message: "Signature verified.".into(),
            subject: Some(SUBJECT.into()),
            issuer: Some(ISSUER.into()),
            chain: vec![SUBJECT.into(), ISSUER.into(), PCA_DN.into(), ROOT_DN.into()],
        }
    }

    #[test]
    fn the_publishers_signature_passes() {
        assert_eq!(check(&real()), Ok(()));
    }

    #[test]
    fn only_a_valid_signature_counts() {
        let unsigned = Signature { status: "NotSigned".into(), subject: None, issuer: None, chain: Vec::new(), ..real() };
        assert_eq!(check(&unsigned), Err("the installer isn't signed, so it wasn't installed".into()));
        for status in ["HashMismatch", "NotTrusted", "UnknownError", "Incompatible", "NotSupportedFileFormat", "", "valid"] {
            let e = check(&Signature { status: status.into(), ..real() }).unwrap_err();
            assert!(e.contains("isn't valid"), "{status}: {e}");
        }
    }

    #[test]
    fn another_publisher_is_refused() {
        for subject in [
            "CN=Learning Machines LLC, O=Learning Machines LLC, C=US",
            "CN=Learning Machines Inc, O=Someone Else, C=US",
            "CN=Someone Else, O=Learning Machines Inc, C=US",
            "CN=Learning Machines Inc Evil, O=Learning Machines Inc, C=US",
            "CN=Learning Machines Inc, CN=Evil, O=Learning Machines Inc",
            "O=Learning Machines Inc",
            "",
        ] {
            let sig =
                Signature { subject: Some(subject.into()), chain: vec![subject.into(), ISSUER.into(), PCA_DN.into(), ROOT_DN.into()], ..real() };
            let e = check(&sig).unwrap_err();
            assert!(e.contains("not by Learning Machines Inc"), "{subject}: {e}");
        }
        assert!(check(&Signature { subject: None, ..real() }).is_err());
    }

    #[test]
    fn an_unknown_issuer_is_refused_by_name() {
        let other = "CN=Microsoft ID Verified CS EOC CA 01, O=Microsoft Corporation, C=US";
        let sig = Signature { issuer: Some(other.into()), chain: vec![SUBJECT.into(), other.into(), PCA_DN.into(), ROOT_DN.into()], ..real() };
        let e = check(&sig).unwrap_err();
        assert!(e.contains("unknown issuer") && e.contains("EOC CA 01"), "{e}");
        // The right name, but not Microsoft's.
        let fake = "CN=Microsoft ID Verified CS AOC CA 04, O=Evil Corp";
        let sig = Signature { issuer: Some(fake.into()), chain: vec![SUBJECT.into(), fake.into(), PCA_DN.into(), ROOT_DN.into()], ..real() };
        assert!(check(&sig).unwrap_err().contains("unknown issuer"));
    }

    #[test]
    fn the_chain_must_run_through_the_pca_to_the_root() {
        let other_root = "CN=Some Other Root, O=Microsoft Corporation";
        for chain in [
            vec![],
            vec![SUBJECT, ISSUER],
            vec![SUBJECT, ISSUER, ROOT_DN],
            vec![SUBJECT, ISSUER, PCA_DN],
            vec![SUBJECT, ISSUER, PCA_DN, other_root],
            vec![SUBJECT, ISSUER, "CN=Microsoft ID Verified Code Signing PCA 2021, O=Evil Corp", ROOT_DN],
            vec![SUBJECT, ISSUER, PCA_DN, ROOT_DN, ROOT_DN],
            vec![ISSUER, SUBJECT, PCA_DN, ROOT_DN],
            vec!["CN=Learning Machines Inc, O=Learning Machines Inc", ISSUER, PCA_DN, ROOT_DN],
        ] {
            let sig = Signature { chain: chain.iter().map(|s| s.to_string()).collect(), ..real() };
            let e = check(&sig).unwrap_err();
            assert!(e.contains("doesn't run through"), "{chain:?}: {e}");
        }
    }

    #[test]
    fn reads_what_powershell_prints() {
        let json = format!(
            r#"{{"status":"Valid","message":"Signature verified.","subject":"{SUBJECT}","issuer":"{ISSUER}","chain":["{SUBJECT}","{ISSUER}","{PCA_DN}","{ROOT_DN}"]}}"#
        );
        assert_eq!(parse(&format!("\r\n{json}\r\n")), Ok(real()));
        // An unsigned file: no certificate, an empty chain.
        let unsigned =
            parse(r#"{"status":"NotSigned","message":"The file is not digitally signed.","subject":null,"issuer":null,"chain":[]}"#).unwrap();
        assert_eq!((unsigned.status.as_str(), unsigned.subject, unsigned.chain.len()), ("NotSigned", None, 0));
        // A one-element array that PowerShell printed as a plain string.
        assert_eq!(parse(r#"{"status":"Valid","chain":"CN=x"}"#).unwrap().chain, ["CN=x"]);
        for junk in ["", "not json", "[]", "{"] {
            if let Ok(sig) = parse(junk) {
                assert!(check(&sig).is_err(), "{junk}");
            }
        }
    }

    #[test]
    fn names_with_quoted_commas_are_read_whole() {
        assert!(is_name(r#"CN="Learning Machines Inc", O="Learning Machines Inc""#, PUBLISHER_CN, PUBLISHER_O));
        assert!(!is_name(r#"CN="Learning Machines Inc, O=Learning Machines Inc""#, PUBLISHER_CN, PUBLISHER_O));
        assert_eq!(rdns(r#"O="A ""B"", Inc", CN=x"#), [("O".to_string(), r#"A "B", Inc"#.to_string()), ("CN".to_string(), "x".to_string())]);
    }

    /// Windows itself, end to end: a file that isn't signed never gets past the check.
    #[cfg(windows)]
    #[test]
    fn windows_refuses_an_unsigned_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photocraft-0.9.0-windows-x64.msi");
        std::fs::write(&file, b"not an installer").unwrap();
        let e = verify(&file).unwrap_err();
        assert!(e.contains("wasn't installed"), "{e}");
    }

    #[test]
    fn the_script_takes_the_file_only_from_the_environment() {
        assert!(!SCRIPT.contains('"'), "Windows would mangle double quotes passed to -Command");
        assert!(SCRIPT.contains(&format!("$env:{FILE_VAR}")));
        assert!(SCRIPT.contains("Get-AuthenticodeSignature -LiteralPath"));
        // Time checks are off only for the chain used to read names, never for validity.
        assert!(SCRIPT.contains("IgnoreNotTimeValid") && SCRIPT.contains("$s.Status.ToString()"));
    }
}
