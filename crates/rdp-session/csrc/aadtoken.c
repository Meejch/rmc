/* Entra ID access token for RDS AAD with a refresh token cache, like Microsoft's own clients
 * (WAM on Windows, MSAL elsewhere): a stored refresh token yields new access tokens without any
 * window; only without one (or when Entra rejects it) the login helper runs the interactive
 * sign-in for the openid scopes with offline_access, and the refresh token obtained there is
 * redeemed for the session host. Fallback: FreeRDP's flow without a stored sign-in.
 *
 * The refresh token is kept in the Secret Service (KWallet on KDE), else encrypted with
 * systemd-creds (secret.c), attributes application=RDPC_SECRET_APP, account=<UPN>, host=<server>;
 * it never touches a plain file or a log.
 * The login helper speaks FreeRDP's auth helper protocol (newline-delimited JSON-RPC over two
 * pipes, see client/common/aad-helper-protocol.md in FreeRDP). */
#include "aadtoken.h"
#include "secret.h"

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <winpr/json.h>
#include <winpr/string.h>
#include <freerdp/client.h>
#include <freerdp/settings.h>
#include <freerdp/utils/aad.h>
#include <freerdp/utils/http.h>
#include <freerdp/log.h>

#define TAG "rmc.aad"
/* No offline_access together with the session host scope: Entra rejects that for FreeRDP's
 * AVD client id (AADSTS65002, live test 24.11.2026). The refresh token comes from a sign-in
 * for the openid scopes instead and is then redeemed for the session host. */
#define OPENID_SCOPE "openid%20profile%20offline_access"
#define NAVIGATE_TIMEOUT_MS 180000

extern char** environ;

/* ---- small helpers ------------------------------------------------------------------- */

static void zfree(char* s)
{
	if (s)
	{
		memset(s, 0, strlen(s));
		free(s);
	}
}

/* ---- stored sign-in (secret.c: Secret Service or systemd-creds) ---------------------- */

static char* secret_lookup(const char* account, const char* host)
{
	const rdpc_attr a[] = { { "application", RDPC_SECRET_APP },
		                    { "account", account },
		                    { "host", host } };
	return rdpc_secret_lookup(a, 3);
}

static void secret_store(const char* account, const char* host, const char* value)
{
	const rdpc_attr a[] = { { "application", RDPC_SECRET_APP },
		                    { "account", account },
		                    { "host", host } };
	if (rdpc_secret_store(a, 3, "RMC: Entra-Anmeldung", value) == RDPC_SECRET_NONE)
		WLog_WARN(TAG, "refresh token could not be stored");
}

static void secret_clear(const char* account, const char* host)
{
	const rdpc_attr a[] = { { "application", RDPC_SECRET_APP },
		                    { "account", account },
		                    { "host", host } };
	rdpc_secret_clear(a, 3);
}

/* ---- token endpoint ------------------------------------------------------------------ */

/* POSTs body to the token endpoint; on success returns the access token and sets *refresh
 * (may stay NULL if Entra sent none). */
static char* token_request(rdpContext* ctx, const char* body, char** refresh, long* status)
{
	*refresh = NULL;
	*status = 0;
	const char* ep = freerdp_utils_aad_get_wellknown_string(ctx, AAD_WELLKNOWN_token_endpoint);
	if (!ep)
		return NULL;
	BYTE* resp = NULL;
	size_t resp_len = 0;
	if (!freerdp_http_request(ep, body, status, &resp, &resp_len))
		return NULL;
	char* access = NULL;
	if (*status == 200 && resp)
	{
		WINPR_JSON* j = WINPR_JSON_ParseWithLength((const char*)resp, resp_len);
		if (j)
		{
			const char* a = WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(j, "access_token"));
			const char* r =
			    WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(j, "refresh_token"));
			access = a ? _strdup(a) : NULL;
			*refresh = r ? _strdup(r) : NULL;
			WINPR_JSON_Delete(j);
		}
	}
	else if (resp)
	{
		/* Entra's error code only (e.g. invalid_grant), never the body with details. */
		WINPR_JSON* j = WINPR_JSON_ParseWithLength((const char*)resp, resp_len);
		const char* err = j ? WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(j, "error")) : NULL;
		/* error_description: AADSTS code and text, no credentials */
		const char* desc =
		    j ? WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(j, "error_description")) : NULL;
		WLog_INFO(TAG, "token endpoint answered %ld (%s): %s", *status, err ? err : "?",
		          desc ? desc : "");
		WINPR_JSON_Delete(j);
	}
	if (resp)
	{
		memset(resp, 0, resp_len);
		free(resp);
	}
	return access;
}

static char* refresh_grant(rdpContext* ctx, const char* refresh, const char* scope,
                           const char* req_cnf, char** new_refresh, long* status)
{
	const char* client_id = freerdp_settings_get_string(ctx->settings, FreeRDP_GatewayAvdClientID);
	if (!client_id)
		return NULL;
	char* enc = winpr_str_url_encode(refresh, strlen(refresh));
	if (!enc)
		return NULL;
	char* body = NULL;
	size_t len = 0;
	winpr_asprintf(&body, &len,
	               "grant_type=refresh_token&client_id=%s&refresh_token=%s&scope=%s&req_cnf=%s",
	               client_id, enc, scope, req_cnf);
	zfree(enc);
	if (!body)
		return NULL;
	char* access = token_request(ctx, body, new_refresh, status);
	zfree(body);
	return access;
}

/* ---- login helper (interactive) ------------------------------------------------------ */

typedef struct
{
	pid_t pid;
	int to;   /* our end: requests */
	int from; /* our end: responses */
	char buf[16384];
	size_t len;
} Helper;

static BOOL helper_send(Helper* h, const char* line)
{
	size_t left = strlen(line);
	const char* p = line;
	while (left > 0)
	{
		const ssize_t n = write(h->to, p, left);
		if (n < 0 && errno == EINTR)
			continue;
		if (n <= 0)
			return FALSE;
		p += n;
		left -= (size_t)n;
	}
	return write(h->to, "\n", 1) == 1;
}

/* Next line (malloc'd) within timeout_ms, NULL on timeout/EOF. */
static char* helper_line(Helper* h, int timeout_ms)
{
	struct timespec t0;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	for (;;)
	{
		char* nl = memchr(h->buf, '\n', h->len);
		if (nl)
		{
			const size_t n = (size_t)(nl - h->buf);
			char* line = calloc(n + 1, 1);
			if (line)
				memcpy(line, h->buf, n);
			memmove(h->buf, nl + 1, h->len - n - 1);
			h->len -= n + 1;
			return line;
		}
		if (h->len >= sizeof(h->buf) - 1)
			return NULL;
		struct timespec t1;
		clock_gettime(CLOCK_MONOTONIC, &t1);
		const long spent = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
		if (spent >= timeout_ms)
			return NULL;
		struct pollfd pfd = { h->from, POLLIN, 0 };
		const int pr = poll(&pfd, 1, (int)(timeout_ms - spent));
		if (pr < 0 && errno == EINTR)
			continue;
		if (pr <= 0)
			return NULL;
		const ssize_t n = read(h->from, h->buf + h->len, sizeof(h->buf) - 1 - h->len);
		if (n < 0 && errno == EINTR)
			continue;
		if (n <= 0)
			return NULL;
		h->len += (size_t)n;
	}
}

static BOOL helper_start(Helper* h, const char* exe)
{
	memset(h, 0, sizeof(*h));
	int req[2], resp[2];
	if (pipe(req) != 0)
		return FALSE;
	if (pipe(resp) != 0)
	{
		close(req[0]);
		close(req[1]);
		return FALSE;
	}
	char in_arg[64], out_arg[64];
	snprintf(in_arg, sizeof(in_arg), "--cmdInFd=P%x", req[0]);
	snprintf(out_arg, sizeof(out_arg), "--cmdOutFd=P%x", resp[1]);
	char* argv[] = { (char*)exe, in_arg, out_arg, NULL };
	posix_spawn_file_actions_t fa;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_addclose(&fa, req[1]);
	posix_spawn_file_actions_addclose(&fa, resp[0]);
	const int rc = posix_spawn(&h->pid, exe, &fa, NULL, argv, environ);
	posix_spawn_file_actions_destroy(&fa);
	close(req[0]);
	close(resp[1]);
	h->to = req[1];
	h->from = resp[0];
	if (rc != 0)
	{
		close(h->to);
		close(h->from);
		WLog_ERR(TAG, "cannot start login helper %s", exe);
		return FALSE;
	}
	return TRUE;
}

static void helper_stop(Helper* h)
{
	helper_send(h, "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"shutdown\"}");
	free(helper_line(h, 2000));
	helper_send(h, "{\"jsonrpc\":\"2.0\",\"method\":\"exit\"}");
	close(h->to);
	close(h->from);
	for (int i = 0; i < 30; i++)
	{
		if (waitpid(h->pid, NULL, WNOHANG) == h->pid)
			return;
		usleep(100000);
	}
	kill(h->pid, SIGTERM);
	waitpid(h->pid, NULL, 0);
}

/* Value of query parameter name in url (URL-decoded), or NULL. */
static char* query_param(const char* url, const char* name)
{
	const char* q = strchr(url, '?');
	const size_t nl = strlen(name);
	for (const char* p = q; p && *p; p = strchr(p + 1, '&'))
	{
		const char* k = p + 1;
		if (strncmp(k, name, nl) == 0 && k[nl] == '=')
		{
			const char* v = k + nl + 1;
			const size_t vl = strcspn(v, "&#");
			return winpr_str_url_decode(v, vl);
		}
	}
	return NULL;
}

/* Starts the helper and does the hello handshake. */
static BOOL helper_open(Helper* h, const char* exe)
{
	if (!helper_start(h, exe))
		return FALSE;
	WINPR_JSON* hello = WINPR_JSON_CreateObject();
	WINPR_JSON_AddStringToObject(hello, "jsonrpc", "2.0");
	WINPR_JSON_AddNumberToObject(hello, "id", 1);
	WINPR_JSON_AddStringToObject(hello, "method", "hello");
	WINPR_JSON* hp = WINPR_JSON_AddObjectToObject(hello, "params");
	WINPR_JSON_AddNumberToObject(hp, "protocol_version", 1);
	WINPR_JSON_AddStringToObject(hp, "client", "rmc");
	char* line = WINPR_JSON_PrintUnformatted(hello);
	WINPR_JSON_Delete(hello);
	BOOL ok = line && helper_send(h, line);
	free(line);
	char* answer = ok ? helper_line(h, 10000) : NULL;
	ok = answer && strstr(answer, "\"result\"");
	free(answer);
	if (!ok)
		helper_stop(h);
	return ok;
}

/* Authorization code flow for scope in the (open) helper: returns the code and the
 * redirect_uri used (for the token request), or NULL (*idp_error set if Entra refused). */
static char* interactive_code(rdpClientContext* cctx, Helper* h, int id, const char* scope,
                              char** redirect_out, BOOL* idp_error)
{
	*redirect_out = NULL;
	*idp_error = FALSE;
	rdpSettings* st = cctx->context.settings;
	char* url = freerdp_client_get_aad_url(cctx, FREERDP_CLIENT_AAD_AUTH_REQUEST, scope);
	if (!url)
		return NULL;
	char* redirect = query_param(url, "redirect_uri");
	char* code = NULL;
	if (!redirect)
		goto out;
	WINPR_JSON* nav = WINPR_JSON_CreateObject();
	WINPR_JSON_AddStringToObject(nav, "jsonrpc", "2.0");
	WINPR_JSON_AddNumberToObject(nav, "id", id);
	WINPR_JSON_AddStringToObject(nav, "method", "navigate");
	WINPR_JSON* np = WINPR_JSON_AddObjectToObject(nav, "params");
	WINPR_JSON_AddStringToObject(np, "title", "Anmeldung (RMC)");
	WINPR_JSON_AddStringToObject(np, "url", url);
	WINPR_JSON_AddStringToObject(np, "redirect_uri", redirect);
	const char* ua = freerdp_settings_get_string(st, FreeRDP_AadAuthUserAgent);
	if (ua && ua[0])
		WINPR_JSON_AddStringToObject(np, "user_agent", ua);
	WINPR_JSON_AddNumberToObject(np, "timeout_ms", NAVIGATE_TIMEOUT_MS);
	char* line = WINPR_JSON_PrintUnformatted(nav);
	WINPR_JSON_Delete(nav);
	const BOOL ok = line && helper_send(h, line);
	free(line);
	char* answer = ok ? helper_line(h, NAVIGATE_TIMEOUT_MS + 5000) : NULL;
	WINPR_JSON* j = answer ? WINPR_JSON_Parse(answer) : NULL;
	const WINPR_JSON* res = j ? WINPR_JSON_GetObjectItem(j, "result") : NULL;
	const char* rurl =
	    res ? WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(res, "redirect_url")) : NULL;
	if (rurl)
		code = freerdp_client_extract_aad_code(cctx, rurl, strlen(rurl));
	else
	{
		const WINPR_JSON* e = j ? WINPR_JSON_GetObjectItem(j, "error") : NULL;
		const char* m = e ? WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(e, "message")) : NULL;
		WLog_WARN(TAG, "login not completed: %s", m ? m : "no answer from the login helper");
		/* Entra refused the request (not cancelled by the user, no timeout) */
		*idp_error = m && strcmp(m, "user_cancelled") != 0 && strcmp(m, "timeout") != 0 &&
		             strcmp(m, "shutting_down") != 0;
	}
	WINPR_JSON_Delete(j);
	zfree(answer);
	if (code)
	{
		*redirect_out = redirect;
		redirect = NULL;
	}
out:
	free(redirect);
	winpr_zfree(url);
	return code;
}

/* Exchanges a code obtained for the openid scopes: returns the refresh token or NULL. The
 * request comes from FreeRDP's builder, which adds the PKCE code_verifier of the preceding
 * authorization request (AADSTS50148 without it). */
static char* openid_refresh_token(rdpClientContext* cctx, const char* code, const char* req_cnf)
{
	char* body = freerdp_client_get_aad_url(cctx, FREERDP_CLIENT_AAD_TOKEN_REQUEST, OPENID_SCOPE,
	                                        code, req_cnf);
	if (!body)
		return NULL;
	char* refresh = NULL;
	long status = 0;
	char* access = token_request(&cctx->context, body, &refresh, &status);
	winpr_zfree(body);
	zfree(access); /* only the refresh token is of interest here */
	return refresh;
}

/* ---- entry point --------------------------------------------------------------------- */

BOOL rdpc_aad_access_token(freerdp* instance, const char* helper_exe, const char* scope,
                           const char* req_cnf, char** token)
{
	*token = NULL;
	rdpContext* ctx = instance->context;
	rdpClientContext* cctx = (rdpClientContext*)ctx;
	const char* account = freerdp_settings_get_string(ctx->settings, FreeRDP_Username);
	const char* host = freerdp_settings_get_string(ctx->settings, FreeRDP_ServerHostname);
	if (!account || !host || !scope || !req_cnf || !helper_exe)
		return FALSE;

	/* 1. silently with the stored refresh token */
	char* refresh = secret_lookup(account, host);
	if (refresh)
	{
		char* fresh = NULL;
		long status = 0;
		*token = refresh_grant(ctx, refresh, scope, req_cnf, &fresh, &status);
		zfree(refresh);
		if (*token)
		{
			if (fresh)
				secret_store(account, host, fresh);
			zfree(fresh);
			WLog_INFO(TAG, "access token from the stored sign-in (no login window)");
			return TRUE;
		}
		zfree(fresh);
		if (status == 400 || status == 401)
			secret_clear(account, host); /* expired or revoked: interactive below */
	}

	Helper h;
	if (!helper_open(&h, helper_exe))
		return FALSE;

	/* 2. interactive sign-in for the openid scopes with offline_access (refresh token, valid
	 *    for all resources of the client, as MSAL does), then the access token silently */
	char* redirect = NULL;
	BOOL idp_error = FALSE;
	char* code = interactive_code(cctx, &h, 2, OPENID_SCOPE, &redirect, &idp_error);
	if (code)
	{
		refresh = openid_refresh_token(cctx, code, req_cnf);
		zfree(code);
		free(redirect);
		if (!refresh)
			WLog_WARN(TAG, "openid sign-in returned no refresh token");
		if (refresh)
		{
			char* fresh = NULL;
			long status = 0;
			*token = refresh_grant(ctx, refresh, scope, req_cnf, &fresh, &status);
			if (!*token)
				WLog_WARN(TAG, "refresh token not accepted for the session host (%ld)", status);
			else
				secret_store(account, host, fresh ? fresh : refresh);
			zfree(fresh);
			zfree(refresh);
			if (*token)
			{
				WLog_INFO(TAG, "sign-in stored in the Secret Service");
				helper_stop(&h);
				return TRUE;
			}
		}
		WLog_WARN(TAG, "refresh token path failed, signing in for the session host directly");
	}
	else if (!idp_error)
	{
		helper_stop(&h); /* cancelled or timed out: respect it */
		return FALSE;
	}

	/* 3. fallback: FreeRDP's flow (code for the session host scope, no stored sign-in) */
	code = interactive_code(cctx, &h, 3, scope, &redirect, &idp_error);
	helper_stop(&h);
	free(redirect);
	if (!code)
		return FALSE;
	char* body =
	    freerdp_client_get_aad_url(cctx, FREERDP_CLIENT_AAD_TOKEN_REQUEST, scope, code, req_cnf);
	zfree(code);
	if (!body)
		return FALSE;
	char* fresh = NULL;
	long status = 0;
	*token = token_request(ctx, body, &fresh, &status);
	winpr_zfree(body);
	if (fresh)
		secret_store(account, host, fresh);
	zfree(fresh);
	return *token != NULL;
}
