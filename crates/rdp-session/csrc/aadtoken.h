/* Entra ID access token with refresh token cache (aadtoken.c). */
#ifndef RDPC_AADTOKEN_H
#define RDPC_AADTOKEN_H

#include <freerdp/freerdp.h>

/* RDS AAD access token for scope/req_cnf (FreeRDP's ACCESS_TOKEN_TYPE_AAD arguments): from the
 * stored refresh token if possible, else interactively via the login helper helper_exe. */
BOOL rdpc_aad_access_token(freerdp* instance, const char* helper_exe, const char* scope,
                           const char* req_cnf, char** token);

#endif
