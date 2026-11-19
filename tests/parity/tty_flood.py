"""M8 regression: Cancel while an owned TTY remains continuously readable."""
import os
from pathlib import Path
import runpy
import signal
import threading
import time
import auth
import frontend_io
import performance_metrics as metrics
import protocol
import run as common
import sessions
import session_cases as journal

_vt = runpy.run_path(str(Path(__file__).resolve().parent.parent/'cli_progress_pty.py'))
Terminal = _vt['Terminal']
footer_phase = _vt['footer_phase']

REPEATS=3
def need(value,message):
    if not value:raise ValueError('TTY flood: '+message)


class CancelAck:
    """Observe the first read whose visible footer acknowledges this signal."""
    def __init__(self, signal_row):
        need(signal_row.get('number') == signal.SIGINT, 'Cancel signal')
        self.offset = signal_row.get('offset')
        self.signal_ns = signal_row.get('at_ns')
        need(type(self.offset) is int and self.offset >= 0 and
             type(self.signal_ns) is int and self.signal_ns > 0, 'Cancel signal clock/offset')
        self.terminal = Terminal()
        self.count = self.end = self.clock = 0
        self.boundary = False
        self.ack_ns = None

    def _signal_boundary(self):
        if not self.boundary and self.end == self.offset:
            need(not footer_phase(self.terminal, 'cancelled'), 'Cancel phase already visible before signal')
            self.boundary = True

    def update(self, trace, raw):
        reads = trace.get('reads')
        need(isinstance(reads, list) and len(reads) >= self.count and len(raw) >= self.offset,
             'read trace inventory')
        self._signal_boundary()
        for read in reads[self.count:]:
            need(isinstance(read, dict) and set(read) == {'begin', 'end', 'at_ns'} and
                 all(type(read[key]) is int for key in read), 'read trace fields')
            begin, end, at = read['begin'], read['end'], read['at_ns']
            need(begin == self.end and begin < end <= len(raw) and at >= self.clock and at > 0,
                 'read trace coverage/clocks')
            if end <= self.offset:
                need(at <= self.signal_ns, 'read before signal clock')
            else:
                need(self.boundary and begin >= self.offset and at >= self.signal_ns,
                     'read after signal clock/offset')
            self.terminal.update(raw[:end])
            self.end, self.clock = end, at
            self.count += 1
            if end <= self.offset:
                self._signal_boundary()
            elif self.ack_ns is None and footer_phase(self.terminal, 'cancelled'):
                self.ack_ns = at
        need(self.end == len(raw) and self.boundary, 'incomplete read trace')


def cancel_ack(trace, raw, signal_row):
    observed = CancelAck(signal_row)
    observed.update(trace, raw)
    need(observed.ack_ns is not None, 'missing visible Cancel acknowledgement')
    return observed.ack_ns


def capture(binary,directory):
    result={'binary':{'path':str(binary),'sha256':common.sha(binary)},'runs':[]}
    for index in range(REPEATS):
        temporary,root=metrics.owned_root();screen=None;thread=None;stop=threading.Event();written=[0];row={'sample':index,'root':str(root),'status':'FAIL'};result['runs'].append(row)
        try:
            root,home,cwd=frontend_io.fixture(root);home.chmod(0o700);cwd.chmod(0o700)
            # Popen/preexec completes before starting this driver's only thread.
            screen=metrics.ObservedPty(str(binary),root,home,cwd,'cancel',True);screen.wait('ready');screen.send('question\r');frontend_io.wait_file(root/'generated')
            def flood():
                while not stop.is_set():
                    try:written[0]+=os.write(screen.master,b'z'*4096)
                    except BlockingIOError:stop.wait(.0001)
                    except OSError:break
            thread=threading.Thread(target=flood);thread.start();deadline=time.monotonic()+2
            while written[0]<32768:
                need(time.monotonic()<deadline and screen.child.poll() is None,'input not saturated');screen.drain(.001)
            row['written_before_cancel']=written[0];screen.signal(signal.SIGINT);sig=screen.trace['signals'][-1];begin=sig['at_ns'];deadline=time.monotonic()+1
            observed=CancelAck(sig);observed.update(screen.trace,screen.raw)
            while observed.ack_ns is None:
                need(time.monotonic()<deadline and screen.child.poll() is None,'Cancel starved by readable input');screen.drain(.001)
                observed.update(screen.trace,screen.raw)
            row['ack_ns']=observed.ack_ns;row['written_at_ack']=written[0]
            need(row['written_at_ack']>row['written_before_cancel'],'no concurrent input during Cancel')
            stop.set();thread.join(timeout=2);need(not thread.is_alive(),'flood thread join')
            row['generated_hex']=(root/'generated').read_bytes().hex();row['snapshot']=sessions.snapshot(root)
            screen.signal(signal.SIGTERM);row['terminal']=screen.finish();need(row['terminal']['exit']==143 and row['terminal']['termios_restored'] and row['terminal']['flags_restored'],'terminal shutdown/restoration')
            row['cancel_seconds']=(row['ack_ns']-begin)/1e9;row['status']='PASS'
        finally:
            stop.set()
            if thread is not None:thread.join(timeout=2)
            if screen is not None:
                row['trace']=screen.trace
                path=directory/f'tty-flood-{index}.pty';path.write_bytes(screen.raw);row['raw']=metrics.artifact(path);screen.close()
            common.write_json(directory/'tty-flood-progress.json',result);temporary.cleanup()
    verify(result,directory,binary);return result
def verify(report,directory,binary):
    need(report['binary']=={'path':str(binary),'sha256':common.sha(binary)},'binary identity')
    need([r['sample'] for r in report['runs']]==list(range(REPEATS)),'closed stress inventory')
    for row in report['runs']:
        need(row['status']=='PASS' and row['generated_hex']=='78','generation/dispatch count')
        root=Path(row['root']);need(root.parent==Path('/tmp') and root.name.startswith('ansa-m4-'),'owned root')
        events=journal.decode(sessions.parts(row['snapshot']));need([e['kind'] for e in events]==['init','accepted','cancel'],'dispatch after Cancel')
        need(events[1]['text']=='question' and events[1]['request']=='request_1' and events[1]['turn']==events[2]['turn'],'actual cancelled turn')
        raw=protocol.owned_bytes(directory/row['raw']['path'],32*1024*1024,single_link=True);need(common.sha(directory/row['raw']['path'])==row['raw']['sha256'] and row['terminal']['stdout_hex']==raw.hex(),'raw PTY output')
        trace=row['trace'];need(trace['argv']==[str(binary),'--frontend-fixture',str(root),'cancel'] and trace['popen_identity'][0]==trace['parent_pid'] and trace['popen_pid']!=trace['parent_pid'],'actual Popen')
        need([s['number'] for s in trace['signals']]==[signal.SIGINT,signal.SIGTERM] and trace['signals'][1]['at_ns']>=row['ack_ns'],'targeted Cancel before shutdown')
        sig=trace['signals'][0];ack=cancel_ack(trace,raw,sig);need(ack==row['ack_ns'] and row['cancel_seconds']==(ack-sig['at_ns'])/1e9 and 0<row['cancel_seconds']<1,'Cancel acknowledgement')
        need(type(row['written_before_cancel']) is int and row['written_before_cancel']>=32768 and type(row['written_at_ack']) is int and row['written_at_ack']>row['written_before_cancel'],'continuous input witness')
        need(row['terminal']['exit']==trace['exit_code']==143 and row['terminal']['termios_restored'] is True and row['terminal']['flags_restored'] is True,'restored terminal')
