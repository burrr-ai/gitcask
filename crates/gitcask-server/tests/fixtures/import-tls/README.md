These DER files are public, synthetic TLS test data for import-origin.test.
The self-signed certificate is trusted only by the injected unit-test HTTPS
client; production import never loads this CA or private key and provides no
TLS/public-IP bypass. The key is not a deployed credential.
