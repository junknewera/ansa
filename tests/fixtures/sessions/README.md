# Synthetic journal/session v1 fixtures

`golden.json` contains only generated offline requests, identities and events.
The `.log` files are actual C++ production Journal output for those traces;
`minimal.log` contains one `{kind:test,value:1}` event. No user sessions or
credentials were read. Regenerate with the built test-only C++ oracle:

`python tests/parity/session_cases.py build-m4/ansa_session_oracle`

Readers must preserve original bytes, framing, part hashes, native item order,
operation IDs and exact context epochs. Torn-tail tests operate on fresh copies.
