# Synthetic auth fixtures

The RSA private/public DER pair was generated solely for offline tests in M3.
It has no relationship to any account or deployed signing key. Never use it
outside tests. Tokens signed with this key use fixture_subject/fixture_client
and an injected clock; they are not bearer credentials for any service.

Private DER is unencrypted PKCS#8. Public DER is SubjectPublicKeyInfo.
