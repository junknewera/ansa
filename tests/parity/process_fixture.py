"""Synthetic process tree/output fixtures. Always run inside the pinned Nix shell."""
import json
import os
from pathlib import Path
import sys
import time

mode=sys.argv[1]
if mode=='output':
    for _ in range(64):
        os.write(1,b'\xff'*4096)
        os.write(2,b'\xff'*4096)
elif mode=='security':
    status=Path('/proc/self/status').read_text()
    value={'no_new_privs':int(next(x.split(':')[1] for x in status.splitlines() if x.startswith('NoNewPrivs:'))),'uid':os.geteuid(),'tty':os.isatty(0)}
    print(json.dumps(value,separators=(',',':')))
elif mode in ('detach','tree'):
    root=Path(sys.argv[2])
    with (root/'launches').open('ab') as record:record.write(b'x')
    child=os.fork()
    if child==0:
        if mode=='detach':os.setsid()
        time.sleep(30)
        os._exit(0)
    (root/'descendant').write_text(str(child))
    (root/'primary').write_text(str(os.getpid()))
    (root/'guardian').write_text(str(os.getppid()))
    print(child,flush=True)
    if mode=='tree':time.sleep(30)
else:
    raise SystemExit('unknown synthetic process fixture')
