/* Secret storage for refresh tokens and passwords (secret.c). */
#ifndef RDPC_SECRET_H
#define RDPC_SECRET_H

#include <stddef.h>

/* Value of the "application" attribute of all stored secrets. Kept stable so stored sign-ins
 * and passwords stay valid across versions. */
#define RDPC_SECRET_APP "rdp-client"

typedef struct
{
	const char* key;
	const char* value;
} rdpc_attr;

/* Where a secret was stored. */
enum
{
	RDPC_SECRET_NONE = 0,
	RDPC_SECRET_SERVICE = 1, /* freedesktop Secret Service (KWallet, GNOME Keyring, KeePassXC) */
	RDPC_SECRET_CREDS = 2    /* file encrypted with systemd-creds (TPM2 and/or host key) */
};

/* Looks up the secret with exactly these attributes (1..4); malloc'd, free with
 * rdpc_secret_free. NULL if there is none. */
char* rdpc_secret_lookup(const rdpc_attr* attrs, size_t n);

/* Stores value under the attributes; returns RDPC_SECRET_SERVICE, RDPC_SECRET_CREDS or
 * RDPC_SECRET_NONE (not stored). Never writes plain text. */
int rdpc_secret_store(const rdpc_attr* attrs, size_t n, const char* label, const char* value);

/* Removes the secret from both stores. */
void rdpc_secret_clear(const rdpc_attr* attrs, size_t n);

/* Removes every secret of this application (attribute application=RDPC_SECRET_APP) and all
 * fallback files (logout). */
void rdpc_secret_clear_all(void);

/* Zeroes and frees a secret returned by rdpc_secret_lookup. */
void rdpc_secret_free(char* s);

#endif
