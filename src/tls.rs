use anyhow::{Context, Result};
use rcgen::generate_simple_self_signed;
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket},
    path::PathBuf,
    sync::Arc,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

pub fn acceptor() -> Result<TlsAcceptor> {
    let directory = dirs::config_dir()
        .context("no config directory")?
        .join("beam");
    let certificate_path = directory.join("certificate.der");
    let key_path = directory.join("private-key.der");
    let (certificate, key) = if certificate_path.exists() && key_path.exists() {
        (fs::read(certificate_path)?, fs::read(key_path)?)
    } else {
        create(&directory, &certificate_path, &key_path)?
    };

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn create(
    directory: &PathBuf,
    certificate_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<(Vec<u8>, Vec<u8>)> {
    fs::create_dir_all(directory)?;
    let mut names = vec![
        "localhost".into(),
        Ipv4Addr::LOCALHOST.to_string(),
        Ipv6Addr::LOCALHOST.to_string(),
    ];
    if let Some(address) = local_ip() {
        names.push(address.to_string());
    }
    let generated = generate_simple_self_signed(names)?;
    let certificate = generated.cert.der().to_vec();
    let key = generated.key_pair.serialize_der();
    fs::write(certificate_path, &certificate)?;
    write_private(key_path, &key)?;
    tracing::info!("created self-signed HTTPS certificate");
    Ok((certificate, key))
}

fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(unix)]
fn write_private(path: &PathBuf, contents: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(contents)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &PathBuf, contents: &[u8]) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}
