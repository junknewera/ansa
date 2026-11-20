// Test-only syscall faults, scoped to one explicitly marked synthetic journal.
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <cstdio>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
namespace {
thread_local bool busy=false;
unsigned writes=0,filesyncs=0,dirsyncs=0;
bool target(int fd,bool directory) {
    const auto root=std::getenv("ANSA_JOURNAL_FAULT_ROOT");if(!root || std::strncmp(root,"/tmp/ansa-m4-",13)!=0)return false;
    char name[64],path[4096];std::snprintf(name,sizeof(name),"/proc/self/fd/%d",fd);const auto n=::readlink(name,path,sizeof(path)-1);if(n<0)return false;path[n]='\0';const auto length=std::strlen(root);
    return directory?std::strcmp(path,root)==0:std::strncmp(path,root,length)==0 && std::strncmp(path+length,"/part-",6)==0;
}
void marker(const char* action) {
    const auto root=std::getenv("ANSA_JOURNAL_FAULT_ROOT");const auto directory=::open(root,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC);if(directory<0)return;
    const auto fd=::openat(directory,"fault-fired",O_WRONLY|O_APPEND|O_CREAT|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC,0600);struct stat s{};
    if(fd>=0 && ::fstat(fd,&s)==0 && S_ISREG(s.st_mode) && s.st_uid==::geteuid() && s.st_nlink==1 && (s.st_mode&077)==0 && s.st_size<4096) {
        (void)::syscall(SYS_write,fd,action,std::strlen(action));(void)::syscall(SYS_write,fd,"\n",1);
    }
    if(fd>=0)::close(fd);::close(directory);
}
// Return an action only at the requested actual syscall, never on stdout/stderr.
const char* action(const char* operation,unsigned count) {
    const auto spec=std::getenv("ANSA_JOURNAL_FAULT");if(!spec)return nullptr;const auto first=std::strchr(spec,':');if(!first || std::strlen(operation)!=static_cast<std::size_t>(first-spec) || std::strncmp(spec,operation,first-spec)!=0)return nullptr;
    char* end=nullptr;const auto nth=std::strtoul(first+1,&end,10);if(!end || *end!=':' || !nth)return nullptr;
    if(count==nth)return end+1;
    if(count==nth+1 && std::strcmp(end+1,"partial_enospc")==0)return "enospc";
    return nullptr;
}
}
extern "C" ssize_t write(int fd,const void* bytes,size_t length) {
    if(busy)return ::syscall(SYS_write,fd,bytes,length);busy=true;
    const auto fault=target(fd,false)?action("write",++writes):nullptr;
    if(fault) {
        marker(fault);
        if(std::strcmp(fault,"short")==0 || std::strcmp(fault,"partial_enospc")==0) {busy=false;return ::syscall(SYS_write,fd,bytes,length>3?3:length);}
        if(std::strcmp(fault,"zero")==0){busy=false;return 0;}
        const auto error=std::strcmp(fault,"eintr")==0?EINTR:std::strcmp(fault,"enospc")==0?ENOSPC:EIO;busy=false;errno=error;return -1;
    }
    busy=false;return ::syscall(SYS_write,fd,bytes,length);
}
extern "C" int fsync(int fd) {
    if(busy)return static_cast<int>(::syscall(SYS_fsync,fd));busy=true;
    const char* fault=nullptr;if(target(fd,false))fault=action("file_sync",++filesyncs);else if(target(fd,true))fault=action("directory_sync",++dirsyncs);
    if(fault){marker(fault);const auto error=std::strcmp(fault,"eintr")==0?EINTR:EIO;busy=false;errno=error;return -1;}
    busy=false;return static_cast<int>(::syscall(SYS_fsync,fd));
}
