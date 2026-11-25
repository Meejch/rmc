/* rmc-login: AAD login helper for RMC (FreeRDP auth helper protocol, see
 * client/common/aad-helper-protocol.md in FreeRDP: newline-delimited JSON-RPC over the pipes
 * given as --cmdInFd=P<hex> / --cmdOutFd=P<hex>).
 *
 * Differences to FreeRDP's freerdp-webview-aad-helper:
 *  - persistent WebKit profile (cookies, "stay signed in") under
 *    $XDG_DATA_HOME/rmc/login (mode 0700), so a later login can run without password
 *    and second factor;
 *  - the window starts hidden and only appears when the login does not complete on its own
 *    within SHOW_DELAY_MS (silent single sign-on shows nothing).
 *
 *   rmc-login --cmdInFd=P<hex> --cmdOutFd=P<hex> [--profile <dir>]
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

#define SHOW_DELAY_MS 1200
#define DEFAULT_TIMEOUT_MS 180000

typedef struct
{
	int out_fd;
	GtkWidget* window;
	WebKitWebView* view;
	GString* inbuf;
	/* in-flight navigate */
	gboolean busy;
	gint64 id;
	char* redirect;
	guint show_timer;
	guint timeout_timer;
} Helper;

static Helper H;

/* ---- minimal JSON (FreeRDP writes compact single-line objects) ------------------------ */

/* Appends the UTF-8 encoding of a code point. */
static void put_utf8(GString* s, gunichar c)
{
	char buf[8];
	const int n = g_unichar_to_utf8(c, buf);
	g_string_append_len(s, buf, n);
}

/* Parses the JSON string starting at p (just after the opening quote). */
static char* json_string_at(const char* p)
{
	GString* s = g_string_new(NULL);
	while (*p && *p != '"')
	{
		if (*p != '\\')
		{
			g_string_append_c(s, *p++);
			continue;
		}
		p++;
		switch (*p)
		{
			case 'b':
				g_string_append_c(s, '\b');
				break;
			case 'f':
				g_string_append_c(s, '\f');
				break;
			case 'n':
				g_string_append_c(s, '\n');
				break;
			case 'r':
				g_string_append_c(s, '\r');
				break;
			case 't':
				g_string_append_c(s, '\t');
				break;
			case 'u':
			{
				char hex[5] = { 0 };
				if (strlen(p + 1) < 4)
					goto out;
				memcpy(hex, p + 1, 4);
				gunichar c = (gunichar)strtoul(hex, NULL, 16);
				p += 4;
				if (c >= 0xD800 && c <= 0xDBFF && p[1] == '\\' && p[2] == 'u' &&
				    strlen(p + 3) >= 4)
				{
					memcpy(hex, p + 3, 4);
					const gunichar lo = (gunichar)strtoul(hex, NULL, 16);
					if (lo >= 0xDC00 && lo <= 0xDFFF)
					{
						c = 0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00);
						p += 6;
					}
				}
				put_utf8(s, c);
				break;
			}
			case '\0':
				goto out;
			default: /* \" \\ \/ */
				g_string_append_c(s, *p);
				break;
		}
		p++;
	}
out:
	return g_string_free(s, FALSE);
}

/* Value position of "key": in obj, or NULL. */
static const char* json_value(const char* obj, const char* key)
{
	char* pat = g_strdup_printf("\"%s\"", key);
	const char* p = obj;
	const char* hit = NULL;
	while ((p = strstr(p, pat)))
	{
		const char* q = p + strlen(pat);
		while (*q == ' ')
			q++;
		if (*q == ':')
		{
			q++;
			while (*q == ' ')
				q++;
			hit = q;
			break;
		}
		p++;
	}
	g_free(pat);
	return hit;
}

static char* json_get_string(const char* obj, const char* key)
{
	const char* v = json_value(obj, key);
	return (v && *v == '"') ? json_string_at(v + 1) : NULL;
}

static gboolean json_get_int(const char* obj, const char* key, gint64* out)
{
	const char* v = json_value(obj, key);
	if (!v || !(g_ascii_isdigit(*v) || *v == '-'))
		return FALSE;
	*out = g_ascii_strtoll(v, NULL, 10);
	return TRUE;
}

static void json_escape(GString* s, const char* v)
{
	g_string_append_c(s, '"');
	for (const unsigned char* p = (const unsigned char*)v; *p; p++)
	{
		if (*p == '"' || *p == '\\')
			g_string_append_printf(s, "\\%c", *p);
		else if (*p < 0x20)
			g_string_append_printf(s, "\\u%04x", *p);
		else
			g_string_append_c(s, (char)*p);
	}
	g_string_append_c(s, '"');
}

/* ---- protocol output ----------------------------------------------------------------- */

static void send_line(GString* s)
{
	g_string_append_c(s, '\n');
	const char* p = s->str;
	size_t left = s->len;
	while (left > 0)
	{
		const ssize_t n = write(H.out_fd, p, left);
		if (n < 0 && errno == EINTR)
			continue;
		if (n <= 0)
			break;
		p += n;
		left -= (size_t)n;
	}
	g_string_free(s, TRUE);
}

static void send_result_raw(gint64 id, const char* result_json)
{
	GString* s = g_string_new(NULL);
	g_string_append_printf(s, "{\"jsonrpc\":\"2.0\",\"id\":%" G_GINT64_FORMAT ",\"result\":%s}", id,
	                       result_json);
	send_line(s);
}

static void send_error(gint64 id, const char* message)
{
	GString* s = g_string_new(NULL);
	g_string_append_printf(s, "{\"jsonrpc\":\"2.0\",\"id\":%" G_GINT64_FORMAT
	                          ",\"error\":{\"code\":1,\"message\":",
	                       id);
	json_escape(s, message);
	g_string_append(s, "}}");
	send_line(s);
}

/* ---- navigate state ------------------------------------------------------------------ */

static void clear_timers(void)
{
	if (H.show_timer)
		g_source_remove(H.show_timer);
	if (H.timeout_timer)
		g_source_remove(H.timeout_timer);
	H.show_timer = H.timeout_timer = 0;
}

/* Ends the in-flight navigate: ok with the redirect URL, or the error message. */
static void finish(const char* redirect_url, const char* error)
{
	if (!H.busy)
		return;
	H.busy = FALSE;
	clear_timers();
	gtk_widget_hide(H.window);
	if (redirect_url)
	{
		GString* r = g_string_new("{\"status\":\"ok\",\"redirect_url\":");
		json_escape(r, redirect_url);
		g_string_append_c(r, '}');
		send_result_raw(H.id, r->str);
		g_string_free(r, TRUE);
	}
	else
		send_error(H.id, error ? error : "user_cancelled");
	g_clear_pointer(&H.redirect, g_free);
	/* Empty page between navigates (the view is reused); events of the old page arriving
	 * later no longer match anything. */
	webkit_web_view_load_uri(H.view, "about:blank");
}

/* The IdP's error parameters of a redirect, "<error>[: <error_subcode>]", or NULL. */
static char* idp_error(const char* uri)
{
	const char* q = strchr(uri, '?');
	if (!q)
		return NULL;
	char* query = g_strdup(q + 1);
	char* hash = strchr(query, '#');
	if (hash)
		*hash = '\0';
	GHashTable* params = g_uri_parse_params(query, -1, "&", G_URI_PARAMS_NONE, NULL);
	g_free(query);
	if (!params)
		return NULL;
	char* out = NULL;
	const char* err = g_hash_table_lookup(params, "error");
	if (err)
	{
		/* The IdP's explanation (AADSTS code and text, no credentials) for the log. */
		const char* desc = g_hash_table_lookup(params, "error_description");
		if (desc)
			fprintf(stderr, "rmc-login: %s: %s\n", err, desc);
		const char* sub = g_hash_table_lookup(params, "error_subcode");
		out = sub ? g_strdup_printf("%s: %s", err, sub) : g_strdup(err);
	}
	g_hash_table_unref(params);
	return out;
}

/* Finishes the navigate if uri is the awaited redirect (URL-decoded prefix, any case). */
static gboolean check_redirect(const char* uri)
{
	if (!H.busy || !uri || !H.redirect)
		return FALSE;
	char* decoded = g_uri_unescape_string(uri, NULL);
	const char* cmp = decoded ? decoded : uri;
	const gboolean match = g_ascii_strncasecmp(cmp, H.redirect, strlen(H.redirect)) == 0;
	g_free(decoded);
	if (!match)
		return FALSE;
	char* err = idp_error(uri);
	if (err)
		finish(NULL, err);
	else
		finish(uri, NULL);
	g_free(err);
	return TRUE;
}

static gboolean on_show_timer(gpointer data)
{
	(void)data;
	H.show_timer = 0;
	if (H.busy)
		gtk_window_present(GTK_WINDOW(H.window));
	return G_SOURCE_REMOVE;
}

static gboolean on_timeout(gpointer data)
{
	(void)data;
	H.timeout_timer = 0;
	finish(NULL, "timeout");
	return G_SOURCE_REMOVE;
}

/* ---- WebKit signals ------------------------------------------------------------------ */

static gboolean on_decide_policy(WebKitWebView* view, WebKitPolicyDecision* decision,
                                 WebKitPolicyDecisionType type, gpointer data)
{
	(void)view;
	(void)data;
	if (type != WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION &&
	    type != WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION)
		return FALSE;
	WebKitNavigationAction* action = webkit_navigation_policy_decision_get_navigation_action(
	    WEBKIT_NAVIGATION_POLICY_DECISION(decision));
	WebKitURIRequest* req = webkit_navigation_action_get_request(action);
	if (check_redirect(webkit_uri_request_get_uri(req)))
	{
		webkit_policy_decision_ignore(decision);
		return TRUE;
	}
	return FALSE;
}

static void on_load_changed(WebKitWebView* view, WebKitLoadEvent ev, gpointer data)
{
	(void)data;
	if (ev == WEBKIT_LOAD_REDIRECTED || ev == WEBKIT_LOAD_COMMITTED)
		check_redirect(webkit_web_view_get_uri(view));
}

static gboolean on_load_failed(WebKitWebView* view, WebKitLoadEvent ev, gchar* failing_uri,
                               GError* error, gpointer data)
{
	(void)view;
	(void)ev;
	(void)data;
	/* Our own policy_decision_ignore() of the redirect reports back here a little later:
	 * not a new redirect (it would answer the next navigate with the old URL). */
	if (g_error_matches(error, WEBKIT_POLICY_ERROR,
	                    WEBKIT_POLICY_ERROR_FRAME_LOAD_INTERRUPTED_BY_POLICY_CHANGE) ||
	    g_error_matches(error, WEBKIT_NETWORK_ERROR, WEBKIT_NETWORK_ERROR_CANCELLED))
		return FALSE;
	/* An unknown scheme (ms-appx-web://) ends up here after an HTTP redirect. */
	return check_redirect(failing_uri);
}

static gboolean on_delete(GtkWidget* w, GdkEvent* ev, gpointer data)
{
	(void)ev;
	(void)data;
	gtk_widget_hide(w);
	finish(NULL, "user_cancelled");
	return TRUE;
}

/* ---- requests ------------------------------------------------------------------------ */

static void do_navigate(gint64 id, const char* line)
{
	if (H.busy)
	{
		send_error(id, "navigate_already_in_progress");
		return;
	}
	char* url = json_get_string(line, "url");
	char* redirect = json_get_string(line, "redirect_uri");
	char* title = json_get_string(line, "title");
	char* ua = json_get_string(line, "user_agent");
	gint64 timeout = 0;
	json_get_int(line, "timeout_ms", &timeout);
	if (!url || !redirect)
	{
		send_error(id, "invalid_params");
		goto out;
	}
	H.busy = TRUE;
	H.id = id;
	char* decoded = g_uri_unescape_string(redirect, NULL);
	H.redirect = decoded ? decoded : g_strdup(redirect);
	WebKitSettings* settings = webkit_web_view_get_settings(H.view);
	webkit_settings_set_user_agent(settings, (ua && *ua) ? ua : NULL);
	gtk_window_set_title(GTK_WINDOW(H.window),
	                     (title && *title) ? title : "Anmeldung (RMC)");
	H.show_timer = g_timeout_add(SHOW_DELAY_MS, on_show_timer, NULL);
	H.timeout_timer = g_timeout_add(timeout > 0 ? (guint)timeout : DEFAULT_TIMEOUT_MS, on_timeout,
	                                NULL);
	webkit_web_view_load_uri(H.view, url);
out:
	g_free(url);
	g_free(redirect);
	g_free(title);
	g_free(ua);
}

static void handle_line(const char* line)
{
	char* method = json_get_string(line, "method");
	gint64 id = -1;
	const gboolean has_id = json_get_int(line, "id", &id);
	if (!method)
		goto out;
	if (strcmp(method, "hello") == 0 && has_id)
		send_result_raw(id, "{\"protocol_version\":1,\"helper\":\"rmc-login/1.0\"}");
	else if (strcmp(method, "navigate") == 0 && has_id)
		do_navigate(id, line);
	else if (strcmp(method, "shutdown") == 0 && has_id)
		send_result_raw(id, "null");
	else if (strcmp(method, "cancel") == 0)
		finish(NULL, "user_cancelled");
	else if (strcmp(method, "exit") == 0)
	{
		finish(NULL, "shutting_down");
		gtk_main_quit();
	}
	else if (has_id)
		send_error(id, "unknown_method");
out:
	g_free(method);
}

static gboolean on_input(GIOChannel* ch, GIOCondition cond, gpointer data)
{
	(void)data;
	char buf[4096];
	gsize n = 0;
	if (cond & G_IO_IN)
	{
		const GIOStatus st = g_io_channel_read_chars(ch, buf, sizeof(buf), &n, NULL);
		if (st == G_IO_STATUS_NORMAL && n > 0)
		{
			g_string_append_len(H.inbuf, buf, (gssize)n);
			char* nl;
			while ((nl = memchr(H.inbuf->str, '\n', H.inbuf->len)))
			{
				*nl = '\0';
				handle_line(H.inbuf->str);
				g_string_erase(H.inbuf, 0, nl - H.inbuf->str + 1);
			}
			return G_SOURCE_CONTINUE;
		}
		if (st == G_IO_STATUS_AGAIN)
			return G_SOURCE_CONTINUE;
	}
	/* FreeRDP went away: end like on "exit". */
	finish(NULL, "shutting_down");
	gtk_main_quit();
	return G_SOURCE_REMOVE;
}

/* ---- setup --------------------------------------------------------------------------- */

static int import_fd(const char* arg)
{
	/* winpr_exportHandleToString() for an anonymous pipe: "P<hex fd>" */
	if (!arg || arg[0] != 'P')
		return -1;
	char* end = NULL;
	const long fd = strtol(arg + 1, &end, 16);
	return (end && *end == '\0' && fd >= 0) ? (int)fd : -1;
}

static char* default_profile(void)
{
	return g_build_filename(g_get_user_data_dir(), "rmc", "login", NULL);
}

/* Creates dir (and parents) and restricts it to the user. */
static gboolean private_dir(const char* dir)
{
	if (g_mkdir_with_parents(dir, 0700) != 0)
		return FALSE;
	return chmod(dir, 0700) == 0;
}

int main(int argc, char** argv)
{
	int in_fd = -1;
	char* profile = NULL;
	H.out_fd = -1;
	for (int i = 1; i < argc; i++)
	{
		if (g_str_has_prefix(argv[i], "--cmdInFd="))
			in_fd = import_fd(argv[i] + 10);
		else if (g_str_has_prefix(argv[i], "--cmdOutFd="))
			H.out_fd = import_fd(argv[i] + 11);
		else if (strcmp(argv[i], "--profile") == 0 && i + 1 < argc)
			profile = g_strdup(argv[++i]);
	}
	if (in_fd < 0 || H.out_fd < 0)
	{
		fprintf(stderr, "usage: %s --cmdInFd=P<hex> --cmdOutFd=P<hex> [--profile <dir>]\n",
		        argv[0]);
		return 2;
	}
	if (!profile)
		profile = default_profile();
	char* data = g_build_filename(profile, "data", NULL);
	char* cache = g_build_filename(profile, "cache", NULL);
	if (!private_dir(profile) || !private_dir(data) || !private_dir(cache))
	{
		fprintf(stderr, "rmc-login: cannot create profile %s\n", profile);
		return 1;
	}

	gtk_init(&argc, &argv);

	WebKitWebsiteDataManager* mgr = webkit_website_data_manager_new(
	    "base-data-directory", data, "base-cache-directory", cache, NULL);
	webkit_website_data_manager_set_itp_enabled(mgr, FALSE);
	char* cookies = g_build_filename(data, "cookies.sqlite", NULL);
	WebKitCookieManager* cm = webkit_website_data_manager_get_cookie_manager(mgr);
	webkit_cookie_manager_set_persistent_storage(cm, cookies, WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE);
	webkit_cookie_manager_set_accept_policy(cm, WEBKIT_COOKIE_POLICY_ACCEPT_ALWAYS);
	WebKitWebContext* ctx = webkit_web_context_new_with_website_data_manager(mgr);

	H.inbuf = g_string_new(NULL);
	H.window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
	gtk_window_set_default_size(GTK_WINDOW(H.window), 520, 680);
	gtk_window_set_title(GTK_WINDOW(H.window), "Anmeldung (RMC)");
	H.view = WEBKIT_WEB_VIEW(webkit_web_view_new_with_context(ctx));
	gtk_container_add(GTK_CONTAINER(H.window), GTK_WIDGET(H.view));
	gtk_widget_show(GTK_WIDGET(H.view)); /* the window itself stays hidden until needed */
	g_signal_connect(H.view, "decide-policy", G_CALLBACK(on_decide_policy), NULL);
	g_signal_connect(H.view, "load-changed", G_CALLBACK(on_load_changed), NULL);
	g_signal_connect(H.view, "load-failed", G_CALLBACK(on_load_failed), NULL);
	g_signal_connect(H.window, "delete-event", G_CALLBACK(on_delete), NULL);

	GIOChannel* ch = g_io_channel_unix_new(in_fd);
	g_io_channel_set_encoding(ch, NULL, NULL);
	g_io_channel_set_buffered(ch, FALSE);
	g_io_add_watch(ch, G_IO_IN | G_IO_HUP | G_IO_ERR, on_input, NULL);

	gtk_main();

	g_io_channel_unref(ch);
	g_free(cookies);
	g_free(data);
	g_free(cache);
	g_free(profile);
	return 0;
}
