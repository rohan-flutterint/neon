# pgxs/neon/Makefile


MODULE_big = neon
OBJS = \
	$(WIN32RES) \
	communicator.o \
	extension_server.o \
	file_cache.o \
	hll.o \
	libpagestore.o \
	logical_replication_monitor.o \
	neon.o \
	neon_lwlsncache.o \
	neon_pgversioncompat.o \
	neon_perf_counters.o \
	neon_utils.o \
	neon_walreader.o \
	pagestore_smgr.o \
	relsize_cache.o \
	unstable_extensions.o \
	walproposer.o \
	walproposer_pg.o \
	neon_ddl_handler.o \
	walsender_hooks.o

PG_CPPFLAGS = -I$(libpq_srcdir)
SHLIB_LINK_INTERNAL = $(libpq)
SHLIB_LINK = -lcurl

EXTENSION = neon
DATA = \
	neon--1.0.sql \
	neon--1.0--1.1.sql \
	neon--1.1--1.2.sql \
	neon--1.2--1.3.sql \
	neon--1.3--1.4.sql \
	neon--1.4--1.5.sql \
	neon--1.5--1.6.sql \
	neon--1.6--1.5.sql \
	neon--1.5--1.4.sql \
	neon--1.4--1.3.sql \
	neon--1.3--1.2.sql \
	neon--1.2--1.1.sql \
	neon--1.1--1.0.sql
PGFILEDESC = "neon - cloud storage for PostgreSQL"

EXTRA_CLEAN = \
	libwalproposer.a

WALPROP_OBJS = \
	$(WIN32RES) \
	walproposer.o \
	neon_utils.o \
	walproposer_compat.o

.PHONY: walproposer-lib
walproposer-lib: CPPFLAGS += -DWALPROPOSER_LIB
walproposer-lib: libwalproposer.a;

.PHONY: libwalproposer.a
libwalproposer.a: $(WALPROP_OBJS)
	$(RM) $@
	$(AR) $(AROPT) $@ $^

# needs vars:
# FIND_TYPEDEF pointing to find_typedef
# INDENT pointing to pg_bsd_indent
# PGINDENT_SCRIPT pointing to pgindent (be careful with PGINDENT var name:
#   pgindent will pick it up as pg_bsd_indent path).
.PHONY: pgindent
pgindent:
	+@ echo top_srcdir=$(top_srcdir) top_builddir=$(top_builddir) srcdir=$(srcdir)
	$(FIND_TYPEDEF) . > neon.typedefs
	INDENT=$(INDENT) $(PGINDENT_SCRIPT) --typedefs neon.typedefs $(srcdir)/*.c $(srcdir)/*.h

PG_CONFIG = pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
