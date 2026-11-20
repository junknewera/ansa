// Test-only glibc interposer. An inherited owned output FD avoids path access.
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fcntl.h>
#include <cerrno>
#include <limits>
#include <sys/stat.h>
#include <unistd.h>
extern "C" void* __libc_malloc(std::size_t) noexcept;
extern "C" void* __libc_calloc(std::size_t,std::size_t) noexcept;
extern "C" void* __libc_realloc(void*,std::size_t) noexcept;
extern "C" void __libc_free(void*) noexcept;
static std::uint64_t allocations=0,requested=0;
static int output=-1;
static void count(std::size_t bytes){__atomic_fetch_add(&allocations,1,__ATOMIC_RELAXED);__atomic_fetch_add(&requested,bytes,__ATOMIC_RELAXED);}
extern "C" void* malloc(std::size_t bytes) noexcept{count(bytes);return __libc_malloc(bytes);}
extern "C" void* calloc(std::size_t count_value,std::size_t bytes) noexcept{if(!bytes||count_value<=std::numeric_limits<std::size_t>::max()/bytes)count(count_value*bytes);return __libc_calloc(count_value,bytes);}
extern "C" void* realloc(void* old,std::size_t bytes) noexcept{count(bytes);return __libc_realloc(old,bytes);}
extern "C" void free(void* value) noexcept{__libc_free(value);}
__attribute__((constructor)) static void setup(){
    const auto* value=std::getenv("ANSA_ALLOCATION_FD");if(!value)return;char* end=nullptr;const long fd=std::strtol(value,&end,10);if(!end||*end||fd<3||fd>1048576)return;
    struct stat meta{};if(::fstat(static_cast<int>(fd),&meta)||!S_ISREG(meta.st_mode)||meta.st_uid!=::geteuid()||meta.st_nlink!=1||(meta.st_mode&077)||meta.st_size)return;
    const auto flags=::fcntl(static_cast<int>(fd),F_GETFL);if(flags<0||(flags&O_ACCMODE)!=O_WRONLY)return;
    output=static_cast<int>(fd);(void)::fcntl(output,F_SETFD,FD_CLOEXEC);
}
__attribute__((destructor)) static void publish(){
    if(output<0)return;char bytes[160];const auto size=std::snprintf(bytes,sizeof(bytes),"{\"allocations\":%llu,\"requested_bytes\":%llu}\n",static_cast<unsigned long long>(__atomic_load_n(&allocations,__ATOMIC_RELAXED)),static_cast<unsigned long long>(__atomic_load_n(&requested,__ATOMIC_RELAXED)));
    if(size>0&&static_cast<std::size_t>(size)<sizeof(bytes)){std::size_t at=0;while(at<static_cast<std::size_t>(size)){const auto written=::write(output,bytes+at,static_cast<std::size_t>(size)-at);if(written<0&&errno==EINTR)continue;if(written<=0)break;at+=static_cast<std::size_t>(written);}}
}
