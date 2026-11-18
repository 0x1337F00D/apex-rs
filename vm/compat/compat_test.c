// SPDX-License-Identifier: GPL-2.0-only
/*
 * 32-bit (i386) user-space test of the apex ioctl ABI, exercising the compat
 * path: structures laid out with 4-byte aligned u64, the 36-byte
 * MAP_BUFFER_FLAGS argument and the 4-byte CLEAR_EVENTFD encoding. Built
 * statically against Google's gasket.h/apex.h so the layouts are theirs.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <linux/types.h>
typedef __u64 u64;
typedef __u32 u32;
#include "gasket.h"
#include "apex.h"

static int failures, passes;
#define CHECK(cond, ...) do { if (cond) { passes++; } else { failures++; printf("FAIL compat " __VA_ARGS__); printf(" (errno %d)\n", errno); } } while (0)

static long sysfs_long(const char *attr)
{
	char path[128], buf[64] = {0};
	snprintf(path, sizeof(path), "/sys/class/apex/apex_0/%s", attr);
	int fd = open(path, O_RDONLY);
	if (fd < 0)
		return -1;
	read(fd, buf, sizeof(buf) - 1);
	close(fd);
	return strtol(buf, NULL, 10);
}

int main(void)
{
	printf("compat: sizeof(void *) = %zu, sizeof(gasket_page_table_ioctl_flags) = %zu\n",
	       sizeof(void *), sizeof(struct gasket_page_table_ioctl_flags));
	int fd = open("/dev/apex_0", O_RDWR | O_CLOEXEC);
	CHECK(fd >= 0, "open");

	u64 n = 0;
	CHECK(ioctl(fd, GASKET_IOCTL_NUMBER_PAGE_TABLES, &n) == 0 && n == 1, "NUMBER_PAGE_TABLES");
	struct gasket_page_table_ioctl pt = {0};
	CHECK(ioctl(fd, GASKET_IOCTL_PAGE_TABLE_SIZE, &pt) == 0 && pt.size == 8192, "PAGE_TABLE_SIZE");

	void *buf = NULL;
	CHECK(posix_memalign(&buf, 4096, 16 * 4096) == 0, "posix_memalign");
	memset(buf, 0x5a, 16 * 4096);
	struct gasket_page_table_ioctl_flags mf = {0};
	mf.base.size = 16 * 4096;
	mf.base.host_address = (uintptr_t)buf;
	mf.base.device_address = 0x40000;
	mf.flags = DMA_TO_DEVICE_FLAG;
	CHECK(ioctl(fd, GASKET_IOCTL_MAP_BUFFER_FLAGS, &mf) == 0, "MAP_BUFFER_FLAGS (36-byte struct)");
	CHECK(sysfs_long("node_0_num_mapped_pages") == 16, "mapped pages after MAP_BUFFER_FLAGS");
	struct gasket_page_table_ioctl um = { .size = 16 * 4096, .device_address = 0x40000 };
	CHECK(ioctl(fd, GASKET_IOCTL_UNMAP_BUFFER, &um) == 0, "UNMAP_BUFFER");
	mf.flags = 3 << 1; /* DMA_NONE */
	CHECK(ioctl(fd, GASKET_IOCTL_MAP_BUFFER_FLAGS, &mf) == -1 && errno == EINVAL, "MAP_BUFFER_FLAGS rejects DMA_NONE");

	int efd = eventfd(0, 0);
	struct gasket_interrupt_eventfd ie = { .interrupt = 4, .event_fd = (u64)efd };
	CHECK(ioctl(fd, GASKET_IOCTL_SET_EVENTFD, &ie) == 0, "SET_EVENTFD");
	CHECK(ioctl(fd, GASKET_IOCTL_CLEAR_EVENTFD, 4UL) == 0, "CLEAR_EVENTFD (4-byte encoding)");
	CHECK(ioctl(fd, GASKET_IOCTL_CLEAR_EVENTFD, 13UL) == -1 && errno == EINVAL, "CLEAR_EVENTFD bounds");

	struct gasket_coherent_alloc_config_ioctl cc = { .enable = 1, .size = 0x4000 };
	CHECK(ioctl(fd, GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR, &cc) == 0 && cc.dma_address == 0x1000000,
	      "CONFIG_COHERENT_ALLOCATOR");
	char *coh = mmap(NULL, 0x4000, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)cc.dma_address);
	CHECK(coh != MAP_FAILED, "mmap coherent");
	if (coh != MAP_FAILED) {
		struct gasket_page_table_ioctl cm = { .size = 0x4000, .host_address = (uintptr_t)coh, .device_address = 0x80000 };
		CHECK(ioctl(fd, GASKET_IOCTL_MAP_BUFFER, &cm) == 0, "MAP_BUFFER of coherent memory");
		cm.host_address = 0;
		CHECK(ioctl(fd, GASKET_IOCTL_UNMAP_BUFFER, &cm) == 0, "UNMAP coherent");
		munmap(coh, 0x4000);
	}
	void *regs = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0x48000);
	CHECK(regs != MAP_FAILED, "mmap CSR window");
	struct apex_performance_expectation_ioctl pe = { .performance = APEX_PERFORMANCE_MAX };
	CHECK(ioctl(fd, APEX_IOCTL_PERFORMANCE_EXPECTATION, &pe) == 0, "PERFORMANCE_EXPECTATION");
	CHECK(ioctl(fd, GASKET_IOCTL_RESET) == 0, "RESET");
	/* A live mapping keeps the file (and the ownership) alive. */
	if (regs != MAP_FAILED)
		munmap(regs, 4096);
	close(fd);
	CHECK(sysfs_long("is_device_owned") == 0 && sysfs_long("node_0_num_mapped_pages") == 0, "released after close");

	printf("compat_test: %d/%d passed\n", passes, passes + failures);
	return failures ? 1 : 0;
}
