"""Entry point for the pinned adjacent editor; paths are passed as data."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess


def private_directory(path):
    path.mkdir(mode=0o700,exist_ok=True)
    descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    metadata=os.fstat(descriptor)
    if metadata.st_uid!=os.geteuid() or metadata.st_mode&0o077:
        os.close(descriptor)
        raise ValueError('Каталог workspace должен принадлежать пользователю и иметь права 0700')
    return descriptor


def regular(directory,name):
    descriptor=os.open(name,os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=directory)
    metadata=os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink!=1 or metadata.st_uid!=os.geteuid() or metadata.st_mode&0o077:
        os.close(descriptor)
        raise ValueError('Небезопасный файл состояния панели')
    return descriptor


def remote(address,*arguments):
    # Never inspect rc-pass or any terminal credential store. The supplied
    # local socket must already grant the launcher the required operations.
    try:
        result=subprocess.run(['kitten','@','--to',address,'--use-password=never',*arguments],
                              capture_output=True,timeout=3)
    except (OSError,subprocess.TimeoutExpired) as error:
        raise ValueError('Kitty не подтвердил команду; повторный запуск проверит прежнюю панель') from error
    if result.returncode or len(result.stdout)>2*1024*1024:
        raise ValueError('Kitty remote control недоступен; нужен доступ к указанному локальному socket')
    return result.stdout


def windows(address,match):
    tree=json.loads(remote(address,'ls','--match',match))
    return [(tab,window) for os_window in tree for tab in os_window.get('tabs',[])
            for window in tab.get('windows',[])]


def launch_kitty(socket,program,arguments,editor,address,parent,reopen=False):
    if not address or not address.startswith('unix:/') or not parent or parent<1:
        raise ValueError('Для --kitty нужны --kitty-to unix:/путь и --parent ID (либо KITTY_LISTEN_ON/KITTY_WINDOW_ID)')
    endpoint=Path(address[5:]);metadata=endpoint.lstat()
    private=metadata.st_mode&0o077==0
    if not private:
        parent_directory=os.open(endpoint.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
        try:
            parent_metadata=os.fstat(parent_directory)
            private=parent_metadata.st_uid==os.geteuid() and parent_metadata.st_mode&0o077==0
        finally:os.close(parent_directory)
    if not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid!=os.geteuid() or not private:
        raise ValueError('Kitty socket должен принадлежать пользователю; нужны права 0600 или private каталог 0700')
    cwd=os.getcwd()
    body=dict(socket=str(socket),editor=editor,program=program,arguments=arguments,address=address,parent=parent,cwd=cwd,
              terminal_device=metadata.st_dev,terminal_inode=metadata.st_ino)
    identity=hashlib.sha256(json.dumps(body,sort_keys=True).encode()).hexdigest()
    workspace=private_directory(socket.parent)
    try:
        os.mkdir('kitty-viewers',mode=0o700,dir_fd=workspace)
    except FileExistsError:pass
    finally:os.close(workspace)
    directory=private_directory(socket.parent/'kitty-viewers')
    lock=regular(directory,identity+'.lock')
    try:
        # A second invocation returns promptly; it must not race a launch whose
        # acknowledgement is still in flight.
        try:fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError:raise ValueError('Открытие панели уже выполняется') from None
        name=identity+'.json'
        def save(value):
            temporary=identity+'.'+secrets.token_hex(8)+'.tmp'
            descriptor=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=directory)
            with os.fdopen(descriptor,'w') as output:
                json.dump(value,output);output.flush();os.fsync(output.fileno())
            os.replace(temporary,name,src_dir_fd=directory,dst_dir_fd=directory);os.fsync(directory)
        state=None
        try:
            descriptor=os.open(name,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=directory)
        except FileNotFoundError:pass
        else:
            with os.fdopen(descriptor,'r') as stored:
                metadata=os.fstat(stored.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink!=1 or metadata.st_uid!=os.geteuid() or metadata.st_mode&0o077 or metadata.st_size>4096:
                    raise ValueError('Небезопасный файл состояния панели')
                state=json.load(stored)
            if state.get('version')!=1 or state.get('identity')!=identity or not isinstance(state.get('tag'),str) or len(state['tag'])!=32 or any(c not in '0123456789abcdef' for c in state['tag']):
                raise ValueError('Неизвестный формат состояния панели')
            # Kitty reports an error for an empty --match. Include the exact
            # parent so a healthy endpoint can prove its child is now absent,
            # while still filtering ownership solely by the random child tag.
            matches=windows(address,'var:ansa_workspace='+state['tag']+' or id:'+str(parent))
            owned=[window for _,window in matches if window.get('user_vars',{}).get('ansa_workspace')==state['tag']]
            if len(owned)>1:raise ValueError('Найдено несколько панелей с одним ID; требуется ручная проверка Kitty')
            if owned:
                state.update(status='opened',window_id=owned[0]['id']);save(state)
                return dict(status='existing',window_id=owned[0]['id'])
            if state.get('status')!='opened':
                raise ValueError('Исход прежнего запуска неизвестен; панель пока не найдена. Новый запуск не отправлен')
            if not reopen:return dict(status='closed',window_id=state.get('window_id'),hint='--reopen открывает закрытую панель')
        parents=windows(address,'id:'+str(parent))
        parents=[(tab,window) for tab,window in parents if window.get('id')==parent]
        if len(parents)!=1:raise ValueError('Родительская панель Kitty не найдена')
        tab,_=parents[0]
        # Location=vsplit is honored by Kitty's native splits layout. Changing
        # an unrelated tab layout implicitly would move the user's workspace.
        if tab.get('layout')!='splits':
            raise ValueError('В этой вкладке Kitty нужен layout splits: kitten @ goto-layout splits')
        state=dict(version=1,identity=identity,tag=secrets.token_hex(16),status='pending')
        save(state)
        reply=remote(address,'launch','--type=window','--match=window_id:'+str(parent),
            '--source-window=id:'+str(parent),'--next-to=id:'+str(parent),'--location=vsplit','--keep-focus',
            '--var=ansa_workspace='+state['tag'],'--title=ANSA Neovim','--cwd='+cwd,
            '--env=ANSA_WORKSPACE_SOCKET='+str(socket),'--env=ANSA_WORKSPACE_EDITOR='+('1' if editor else '0'),program,*arguments)
        try:window_id=int(reply.strip());assert window_id>0
        except (ValueError,AssertionError):raise ValueError('Kitty вернул неизвестный исход запуска; повторная команда проверит прежнюю панель') from None
        state.update(status='opened',window_id=window_id);save(state)
        return dict(status='opened',window_id=window_id)
    finally:os.close(lock);os.close(directory)


def main():
    parser=argparse.ArgumentParser(prog="ansa-nvim",description="Открыть Neovim рядом с Carbon workspace")
    parser.add_argument("--socket",required=True,type=Path,help="путь к workspace.sock")
    parser.add_argument("--editor",action="store_true",help="передавать выделение и задачи; owner требует --workspace-editor")
    parser.add_argument("--kitty",action="store_true",help="открыть соседнюю панель в Kitty")
    parser.add_argument("--kitty-to",default=os.environ.get('KITTY_LISTEN_ON'),help="адрес локального Kitty remote-control socket")
    parser.add_argument("--parent",type=int,default=os.environ.get('KITTY_WINDOW_ID'),help="ID родительской панели Kitty")
    parser.add_argument("--reopen",action="store_true",help="заново открыть подтверждённо закрытую панель Kitty")
    parser.add_argument("arguments",nargs=argparse.REMAINDER,help="файлы и аргументы Neovim после --")
    args=parser.parse_args()
    program=os.environ["ANSA_WORKSPACE_NVIM"]
    env=os.environ.copy()
    env["ANSA_WORKSPACE_SOCKET"]=str(args.socket.absolute())
    env["ANSA_WORKSPACE_EDITOR"]="1" if args.editor else "0"
    arguments=args.arguments[1:] if args.arguments[:1]==["--"] else args.arguments
    if args.kitty:
        try:result=launch_kitty(args.socket.absolute(),program,arguments,args.editor,args.kitty_to,args.parent,args.reopen)
        except (ValueError,OSError,json.JSONDecodeError) as error:parser.exit(1,str(error)+'\n')
        print(json.dumps(result,ensure_ascii=False));return
    os.execve(program,[program,*arguments],env)


if __name__=="__main__":
    main()
