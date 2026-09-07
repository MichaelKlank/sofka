"""Generate CA server certificates with the existing public test key."""

from pathlib import Path
import subprocess

DEST = Path(__file__).resolve().parent

variants = {
    "proxy-ca": [],
    "proxy-ca-server-auth": ["extendedKeyUsage=serverAuth"],
    "proxy-ca-client-auth": ["extendedKeyUsage=clientAuth"],
    "proxy-ca-key-cert-sign": ["keyUsage=critical,keyCertSign"],
    "proxy-ca-malformed-ku": ["2.5.29.15=critical,DER:03:02:07:80:05:00"],
    "proxy-ca-critical": ["1.2.3.4=critical,DER:05:00"],
    "proxy-ca-constrained": ["nameConstraints=critical,permitted;DNS:other.example"],
}
for name, extra in variants.items():
    extensions = [
        "basicConstraints=critical,CA:TRUE",
        "subjectAltName=DNS:localhost,DNS:*.proxy.example,IP:127.0.0.1",
    ]
    if name not in ["proxy-ca-key-cert-sign", "proxy-ca-malformed-ku"]:
        extensions.append("keyUsage=critical,digitalSignature,keyCertSign,cRLSign")
    args = [
        "openssl", "req", "-new", "-x509", "-key", str(DEST / "server.key"),
        "-subj", "/CN=localhost/O=sofka test proxy",
        "-not_before", "20200101000000Z", "-not_after", "21200101000000Z",
        "-out", str(DEST / f"{name}.pem"),
    ]
    for extension in extensions + extra:
        args.extend(["-addext", extension])
    subprocess.run(args, check=True, capture_output=True)
