"""Generate public test credentials. Never use these keys outside tests."""

from pathlib import Path
import subprocess
import tempfile

DEST = Path(__file__).resolve().parent


def openssl(*args):
    subprocess.run(["openssl", *args], check=True, capture_output=True)


with tempfile.TemporaryDirectory() as directory:
    tmp = Path(directory)
    ca_key = str(tmp / "ca.key")
    ca_cert = str(DEST / "ca.pem")
    openssl("genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", ca_key)
    openssl("req", "-new", "-x509", "-key", ca_key, "-subj", "/CN=sofka test CA", "-days", "36500",
            "-addext", "basicConstraints=critical,CA:TRUE", "-out", ca_cert)
    (tmp / "index").write_text("")
    (tmp / "serial").write_text("01\n")
    config = tmp / "ca.cnf"
    config.write_text(f"""[ca]
default_ca = test
[test]
database = {tmp / 'index'}
serial = {tmp / 'serial'}
new_certs_dir = {tmp}
certificate = {ca_cert}
private_key = {ca_key}
default_md = sha256
policy = policy
unique_subject = no
[policy]
commonName = supplied
[server]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost
[client]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = clientAuth
""")
    for name in ("client", "server"):
        key = str(DEST / f"{name}.key")
        csr = str(tmp / f"{name}.csr")
        openssl("genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", key)
        openssl("req", "-new", "-key", key, "-subj", f"/CN={name}", "-out", csr)
        variants = [(name, name, "21200101000000Z")]
        if name == "client":
            variants.append(("client-v1", "v1", "21200101000000Z"))
        else:
            variants.append(("server-expired", name, "20210101000000Z"))
        for output, extensions, end in variants:
            if extensions == "v1":
                openssl("req", "-config", str(config), "-in", csr, "-x509", "-x509v1", "-CA", ca_cert,
                        "-CAkey", ca_key, "-not_before", "20200101000000Z",
                        "-not_after", end, "-out", str(DEST / f"{output}.pem"))
                continue
            args = ["ca", "-batch", "-notext", "-config", str(config), "-in", csr,
                    "-startdate", "20200101000000Z", "-enddate", end,
                    "-out", str(DEST / f"{output}.pem")]
            if extensions:
                args.extend(["-extensions", extensions])
            openssl(*args)

# The test server trusts the exact P-521 certificate, so it can be self-signed.
# Tests check private key loading and the TLS handshake signature.
p521_key = str(DEST / "client-p521.key")
p521_cert = str(DEST / "client-p521.pem")
openssl("ecparam", "-name", "secp521r1", "-genkey", "-noout", "-out", p521_key)
openssl("req", "-new", "-x509", "-key", p521_key, "-subj", "/CN=client-p521", "-sha384",
        "-days", "36500", "-out", p521_cert)
