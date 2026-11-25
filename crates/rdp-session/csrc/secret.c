/* Secret storage: the freedesktop Secret Service through
 * libsecret, loaded at runtime (no build dependency, works with KWallet, GNOME Keyring,
 * KeePassXC). Without a Secret Service the secret goes into a file encrypted with
 * systemd-creds (user scope: bound to this machine and user, TPM2 if present). Plain text is
 * never written.
 *
 * Items written by the earlier secret-tool code match as well: the schema does not match by
 * name, only the attributes count. */
#include "secret.h"

#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <freerdp/log.h>

#define TAG "rmc.secret"
#define MAX_ATTRS 4

extern char** environ;

/* ---- libsecret ABI (stable since 0.18, see secret-schema.h) ---------------------------- */

typedef struct
{
	const char* name;
	int type; /* SECRET_SCHEMA_ATTRIBUTE_STRING = 0 */
} SecretSchemaAttribute;

typedef struct
{
	const char* name;
	int flags; /* SECRET_SCHEMA_DONT_MATCH_NAME = 1 << 1 */
	SecretSchemaAttribute attributes[32];
	int reserved;
	void* reserved1;
	void* reserved2;
	void* reserved3;
	void* reserved4;
	void* reserved5;
	void* reserved6;
	void* reserved7;
} SecretSchema;

typedef struct
{
	uint32_t domain;
	int code;
	char* message;
} GError;

typedef int (*store_fn)(const SecretSchema*, const char*, const char*, const char*, void*,
                        GError**, ...);
typedef char* (*lookup_fn)(const SecretSchema*, void*, GError**, ...);
typedef int (*clear_fn)(const SecretSchema*, void*, GError**, ...);
typedef void (*pwfree_fn)(char*);
typedef void (*errfree_fn)(GError*);

static struct
{
	int tried;
	store_fn store;
	lookup_fn lookup;
	clear_fn clear;
	pwfree_fn pwfree;
	errfree_fn errfree;
} lib;

static int lib_load(void)
{
	if (lib.tried)
		return lib.store != NULL;
	lib.tried = 1;
	void* h = dlopen("libsecret-1.so.0", RTLD_NOW | RTLD_LOCAL);
	if (!h)
	{
		WLog_INFO(TAG, "libsecret not available, using systemd-creds");
		return 0;
	}
	lib.store = (store_fn)dlsym(h, "secret_password_store_sync");
	lib.lookup = (lookup_fn)dlsym(h, "secret_password_lookup_sync");
	lib.clear = (clear_fn)dlsym(h, "secret_password_clear_sync");
	lib.pwfree = (pwfree_fn)dlsym(h, "secret_password_free");
	lib.errfree = (errfree_fn)dlsym(h, "g_error_free");
	if (!lib.store || !lib.lookup || !lib.clear || !lib.pwfree || !lib.errfree)
	{
		WLog_WARN(TAG, "libsecret incomplete, using systemd-creds");
		lib.store = NULL;
		return 0;
	}
	return 1;
}

static void schema_of(SecretSchema* s, const rdpc_attr* a, size_t n)
{
	memset(s, 0, sizeof(*s));
	s->name = "rmc.Secret";
	s->flags = 1 << 1;
	for (size_t i = 0; i < n; i++)
		s->attributes[i].name = a[i].key;
}

/* The variadic attribute list: expanded for 1..4 attributes. */
#define V(i) a[i].key, a[i].value
#define CALL_ATTRS(call, ...)                                                   \
	(n == 1   ? call(__VA_ARGS__, V(0), NULL)                                     \
	 : n == 2 ? call(__VA_ARGS__, V(0), V(1), NULL)                               \
	 : n == 3 ? call(__VA_ARGS__, V(0), V(1), V(2), NULL)                         \
	          : call(__VA_ARGS__, V(0), V(1), V(2), V(3), NULL))

static void log_error(const char* what, GError* e)
{
	if (e)
	{
		WLog_INFO(TAG, "Secret Service %s: %s", what, e->message ? e->message : "?");
		lib.errfree(e);
	}
}

/* 1 = found (*out set), 0 = not found, -1 = Secret Service unusable */
static int service_lookup(const rdpc_attr* a, size_t n, char** out)
{
	*out = NULL;
	if (!lib_load())
		return -1;
	SecretSchema s;
	schema_of(&s, a, n);
	GError* e = NULL;
	char* v = CALL_ATTRS(lib.lookup, &s, NULL, &e);
	if (e)
	{
		log_error("lookup", e);
		return -1;
	}
	if (!v)
		return 0;
	*out = strdup(v);
	memset(v, 0, strlen(v));
	lib.pwfree(v);
	return *out ? 1 : -1;
}

static int service_store(const rdpc_attr* a, size_t n, const char* label, const char* value)
{
	if (!lib_load())
		return 0;
	SecretSchema s;
	schema_of(&s, a, n);
	GError* e = NULL;
	const int ok = CALL_ATTRS(lib.store, &s, NULL, label, value, NULL, &e);
	log_error("store", e);
	return ok && !e;
}

static void service_clear(const rdpc_attr* a, size_t n)
{
	if (!lib_load())
		return;
	SecretSchema s;
	schema_of(&s, a, n);
	GError* e = NULL;
	(void)CALL_ATTRS(lib.clear, &s, NULL, &e);
	log_error("clear", e);
}

/* ---- systemd-creds fallback ------------------------------------------------------------ */

/* Runs argv with stdin = in (may be NULL); returns stdout or NULL on failure/non-zero exit. */
static char* run_capture(char* const argv[], const char* in)
{
	int pin[2] = { -1, -1 }, pout[2] = { -1, -1 };
	if (pipe(pin) != 0 || pipe(pout) != 0)
		goto fail;
	posix_spawn_file_actions_t fa;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_adddup2(&fa, pin[0], 0);
	posix_spawn_file_actions_adddup2(&fa, pout[1], 1);
	posix_spawn_file_actions_addclose(&fa, pin[1]);
	posix_spawn_file_actions_addclose(&fa, pout[0]);
	pid_t pid = 0;
	const int rc = posix_spawnp(&pid, argv[0], &fa, NULL, argv, environ);
	posix_spawn_file_actions_destroy(&fa);
	close(pin[0]);
	close(pout[1]);
	pin[0] = pout[1] = -1;
	if (rc != 0)
		goto fail;
	if (in)
	{
		size_t left = strlen(in);
		const char* p = in;
		while (left > 0)
		{
			const ssize_t k = write(pin[1], p, left);
			if (k < 0 && errno == EINTR)
				continue;
			if (k <= 0)
				break;
			p += k;
			left -= (size_t)k;
		}
	}
	close(pin[1]);
	pin[1] = -1;
	size_t cap = 4096, len = 0;
	char* out = calloc(cap, 1);
	while (out)
	{
		if (len + 1 >= cap)
		{
			char* grown = calloc(cap * 2, 1);
			if (grown)
				memcpy(grown, out, len);
			memset(out, 0, cap);
			free(out);
			out = grown;
			cap *= 2;
			continue;
		}
		const ssize_t k = read(pout[0], out + len, cap - len - 1);
		if (k < 0 && errno == EINTR)
			continue;
		if (k <= 0)
			break;
		len += (size_t)k;
	}
	close(pout[0]);
	int status = 0;
	while (waitpid(pid, &status, 0) < 0 && errno == EINTR)
		;
	if (!out || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
	{
		rdpc_secret_free(out);
		return NULL;
	}
	return out;
fail:
	for (int i = 0; i < 2; i++)
	{
		if (pin[i] >= 0)
			close(pin[i]);
		if (pout[i] >= 0)
			close(pout[i]);
	}
	return NULL;
}

/* $XDG_DATA_HOME/rmc/secrets (0700), created on demand. */
static int creds_dir(char* buf, size_t len, int create)
{
	const char* xdg = getenv("XDG_DATA_HOME");
	const char* home = getenv("HOME");
	int w = 0;
	if (xdg && xdg[0])
		w = snprintf(buf, len, "%s/rmc/secrets", xdg);
	else if (home)
		w = snprintf(buf, len, "%s/.local/share/rmc/secrets", home);
	else
		return 0;
	if (w <= 0 || (size_t)w >= len)
		return 0;
	if (create)
	{
		char parent[4096];
		snprintf(parent, sizeof(parent), "%s", buf);
		char* slash = strrchr(parent, '/');
		if (slash)
		{
			*slash = '\0';
			char* p2 = strrchr(parent, '/');
			if (p2)
			{
				*p2 = '\0';
				(void)mkdir(parent, 0700);
				*p2 = '/';
			}
			(void)mkdir(parent, 0700);
		}
		if (mkdir(buf, 0700) != 0 && errno != EEXIST)
			return 0;
		(void)chmod(buf, 0700);
	}
	return 1;
}

/* File name of a secret: FNV-1a over the attributes (only names the file). */
static int creds_path(const rdpc_attr* a, size_t n, char* path, size_t len, char* name,
                      size_t nlen, int create)
{
	char dir[4096];
	if (!creds_dir(dir, sizeof(dir), create))
		return 0;
	uint64_t h = 0xcbf29ce484222325ULL;
	for (size_t i = 0; i < n; i++)
	{
		for (const char* p = a[i].key; *p; p++)
			h = (h ^ (uint8_t)*p) * 0x100000001b3ULL;
		h = (h ^ '=') * 0x100000001b3ULL;
		for (const char* p = a[i].value; *p; p++)
			h = (h ^ (uint8_t)*p) * 0x100000001b3ULL;
		h = (h ^ '\n') * 0x100000001b3ULL;
	}
	snprintf(name, nlen, "rdpc-%016llx", (unsigned long long)h);
	const int w = snprintf(path, len, "%s/%s.cred", dir, name);
	return w > 0 && (size_t)w < len;
}

static char* creds_lookup(const rdpc_attr* a, size_t n)
{
	char path[4200], name[64];
	if (!creds_path(a, n, path, sizeof(path), name, sizeof(name), 0) || access(path, R_OK) != 0)
		return NULL;
	char nameopt[80];
	snprintf(nameopt, sizeof(nameopt), "--name=%s", name);
	char* argv[] = { "systemd-creds", "--user", "decrypt", nameopt, path, "-", NULL };
	return run_capture(argv, NULL);
}

static int creds_store(const rdpc_attr* a, size_t n, const char* value)
{
	char path[4200], name[64];
	if (!creds_path(a, n, path, sizeof(path), name, sizeof(name), 1))
		return 0;
	char tmp[4300], nameopt[80];
	snprintf(tmp, sizeof(tmp), "%s.tmp", path);
	snprintf(nameopt, sizeof(nameopt), "--name=%s", name);
	char* argv[] = { "systemd-creds", "--user", "encrypt", nameopt, "-", tmp, NULL };
	char* out = run_capture(argv, value);
	if (!out)
	{
		(void)unlink(tmp);
		WLog_WARN(TAG, "systemd-creds encrypt failed: secret not stored");
		return 0;
	}
	free(out);
	(void)chmod(tmp, 0600);
	return rename(tmp, path) == 0;
}

static void creds_clear(const rdpc_attr* a, size_t n)
{
	char path[4200], name[64];
	if (creds_path(a, n, path, sizeof(path), name, sizeof(name), 0))
		(void)unlink(path);
}

/* ---- API ------------------------------------------------------------------------------ */

static int valid(const rdpc_attr* a, size_t n)
{
	if (!a || n == 0 || n > MAX_ATTRS)
		return 0;
	for (size_t i = 0; i < n; i++)
		if (!a[i].key || !a[i].value)
			return 0;
	return 1;
}

static int force_creds(void)
{
	/* RDPC_SECRET_BACKEND=creds (tests): skip the Secret Service. */
	const char* b = getenv("RDPC_SECRET_BACKEND");
	return b && strcmp(b, "creds") == 0;
}

char* rdpc_secret_lookup(const rdpc_attr* attrs, size_t n)
{
	if (!valid(attrs, n))
		return NULL;
	char* v = NULL;
	if (!force_creds() && service_lookup(attrs, n, &v) == 1)
		return v;
	return creds_lookup(attrs, n);
}

int rdpc_secret_store(const rdpc_attr* attrs, size_t n, const char* label, const char* value)
{
	if (!valid(attrs, n) || !value)
		return RDPC_SECRET_NONE;
	if (!force_creds() && service_store(attrs, n, label ? label : "RMC", value))
	{
		creds_clear(attrs, n); /* an older fallback copy must not shadow the new value */
		return RDPC_SECRET_SERVICE;
	}
	return creds_store(attrs, n, value) ? RDPC_SECRET_CREDS : RDPC_SECRET_NONE;
}

void rdpc_secret_clear(const rdpc_attr* attrs, size_t n)
{
	if (!valid(attrs, n))
		return;
	if (!force_creds())
		service_clear(attrs, n);
	creds_clear(attrs, n);
}

void rdpc_secret_clear_all(void)
{
	const rdpc_attr a[] = { { "application", RDPC_SECRET_APP } };
	if (!force_creds())
		service_clear(a, 1);
	char dir[4096];
	if (!creds_dir(dir, sizeof(dir), 0))
		return;
	DIR* d = opendir(dir);
	if (!d)
		return;
	struct dirent* e;
	while ((e = readdir(d)) != NULL)
	{
		const size_t l = strlen(e->d_name);
		if (strncmp(e->d_name, "rdpc-", 5) == 0 && l > 5 && strcmp(e->d_name + l - 5, ".cred") == 0)
		{
			char path[4400];
			snprintf(path, sizeof(path), "%s/%s", dir, e->d_name);
			(void)unlink(path);
		}
	}
	closedir(d);
}

void rdpc_secret_free(char* s)
{
	if (s)
	{
		memset(s, 0, strlen(s));
		free(s);
	}
}
