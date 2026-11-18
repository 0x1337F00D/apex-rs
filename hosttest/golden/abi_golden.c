// SPDX-License-Identifier: GPL-2.0-only
/*
 * Prints the gasket/apex ioctl numbers and argument sizes from Google's own
 * headers; abi_golden.txt and abi_golden_i386.txt are its output. With Google's
 * gasket-driver sources in $GASKET (https://github.com/google/gasket-driver):
 *
 *   cc -I$GASKET/src abi_golden.c -o abi_golden && ./abi_golden > abi_golden.txt
 *   (and the same with an i386 compiler for abi_golden_i386.txt)
 */

#include <stdio.h>
#include <stdint.h>
#include <linux/types.h>
typedef __u64 u64; typedef __u32 u32;
#include "gasket.h"
#include "apex.h"
#define P(x) printf("%s 0x%08x\n", #x, (unsigned)(x))
#define S(x) printf("sizeof %s %zu\n", #x, sizeof(struct x))
int main(void){
 P(GASKET_IOCTL_RESET);P(GASKET_IOCTL_SET_EVENTFD);P(GASKET_IOCTL_CLEAR_EVENTFD);P(GASKET_IOCTL_LOOPBACK_INTERRUPT);
 P(GASKET_IOCTL_NUMBER_PAGE_TABLES);P(GASKET_IOCTL_PAGE_TABLE_SIZE);P(GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE);
 P(GASKET_IOCTL_PARTITION_PAGE_TABLE);P(GASKET_IOCTL_MAP_BUFFER);P(GASKET_IOCTL_UNMAP_BUFFER);
 P(GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS);P(GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR);P(GASKET_IOCTL_MAP_BUFFER_FLAGS);
 P(GASKET_IOCTL_MAP_DMABUF);P(APEX_IOCTL_GATE_CLOCK);P(APEX_IOCTL_PERFORMANCE_EXPECTATION);
 S(gasket_interrupt_eventfd);S(gasket_page_table_ioctl);S(gasket_page_table_ioctl_flags);S(gasket_coherent_alloc_config_ioctl);
 S(gasket_page_table_ioctl_dmabuf);S(apex_gate_clock_ioctl);S(apex_performance_expectation_ioctl);
 return 0;}
