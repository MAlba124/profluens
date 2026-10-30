/* libflac-sys config.h. What FLAC 1.5.0's config.cmake.h.in becomes for a Linux build
 * with WITH_OGG=OFF, ENABLE_MULTITHREADING=OFF, ENABLE_64_BIT_WORDS=ON and WITH_AVX=ON,
 * written by hand so the build runs no CMake. The architecture rows follow the target
 * the way CMake's checks would (x86intrin.h exists on x86, arm_neon.h on aarch64). */

#define CPU_IS_BIG_ENDIAN 0
#define WORDS_BIGENDIAN CPU_IS_BIG_ENDIAN
#define ENABLE_64_BIT_WORDS 1

#define OGG_FOUND 0
#define FLAC__HAS_OGG OGG_FOUND

#if defined(__x86_64__) || defined(__i386__)
#define FLAC__ALIGN_MALLOC_DATA
#define FLAC__HAS_X86INTRIN 1
#define HAVE_CPUID_H
#define HAVE_X86INTRIN_H
#else
#define FLAC__HAS_X86INTRIN 0
#endif

#if defined(__aarch64__)
#define FLAC__CPU_ARM64
#define FLAC__HAS_NEONINTRIN 1
#define FLAC__HAS_A64NEONINTRIN 1
#else
#define FLAC__HAS_NEONINTRIN 0
#define FLAC__HAS_A64NEONINTRIN 0
#endif

#define FLAC__SYS_LINUX

#define WITH_AVX
#ifdef WITH_AVX
  #define FLAC__USE_AVX
#endif

#define HAVE_BSWAP16
#define HAVE_BSWAP32
#define HAVE_BYTESWAP_H
#define HAVE_CLOCK_GETTIME
#define HAVE_FSEEKO
#define HAVE_GETOPT_LONG
#define HAVE_INTTYPES_H
#define HAVE_LANGINFO_CODESET
#define HAVE_LROUND 1
#define HAVE_STDINT_H
#define HAVE_STDLIB_H
#define HAVE_STRING_H
#define HAVE_SYS_IOCTL_H
#define HAVE_SYS_PARAM_H
#define HAVE_SYS_STAT_H
#define HAVE_SYS_TIME_H
#define HAVE_SYS_TYPES_H
#define HAVE_TERMIOS_H
#define HAVE_TYPEOF
#define HAVE_UNISTD_H

/* HAVE_PTHREAD stays undefined, the encoder runs on the calling thread. */

/* PACKAGE_VERSION comes from build.rs, its FLAC_RELEASE. */

#ifndef _ALL_SOURCE
#define _ALL_SOURCE
#endif
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#ifndef _POSIX_PTHREAD_SEMANTICS
#define _POSIX_PTHREAD_SEMANTICS
#endif
#ifndef _TANDEM_SOURCE
#define _TANDEM_SOURCE
#endif
#ifndef __EXTENSIONS__
#define __EXTENSIONS__ 1
#endif
#ifndef _FILE_OFFSET_BITS
#define _FILE_OFFSET_BITS 64
#endif
#ifndef _LARGEFILE_SOURCE
#define _LARGEFILE_SOURCE
#endif
