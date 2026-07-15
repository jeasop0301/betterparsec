// BetterParsec distribution worker.
//
// Routes:
//   POST /api/download  password-gated download of the portable zip from R2
//   *                   static landing page (Workers Assets)
//
// The download password lives in the DOWNLOAD_PASSWORD secret
// (`wrangler secret put DOWNLOAD_PASSWORD`).

const DIST_OBJECT = "betterparsec-portable.zip";

/** Constant-time password comparison via SHA-256 digests. */
async function passwordMatches(given, expected) {
    const enc = new TextEncoder();
    const [a, b] = await Promise.all([
        crypto.subtle.digest("SHA-256", enc.encode(given)),
        crypto.subtle.digest("SHA-256", enc.encode(expected)),
    ]);
    const av = new Uint8Array(a);
    const bv = new Uint8Array(b);
    let diff = 0;
    for (let i = 0; i < av.length; i++) {
        diff |= av[i] ^ bv[i];
    }
    return diff === 0;
}

export default {
    async fetch(request, env) {
        const url = new URL(request.url);

        if (url.pathname === "/api/download") {
            if (request.method !== "POST") {
                return new Response("Method Not Allowed", { status: 405 });
            }
            const form = await request.formData();
            const password = String(form.get("password") ?? "");
            const expected = env.DOWNLOAD_PASSWORD;
            if (!expected || !(await passwordMatches(password, expected))) {
                // Bounce back to the download section with an error flag.
                return Response.redirect(`${url.origin}/?error=1#download`, 303);
            }
            const object = await env.DIST.get(DIST_OBJECT);
            if (!object) {
                return new Response(
                    "Build not uploaded yet. Run tools/publish-cf.ps1 on the host.",
                    { status: 404 },
                );
            }
            return new Response(object.body, {
                headers: {
                    "content-type": "application/zip",
                    "content-length": String(object.size),
                    "content-disposition": `attachment; filename="${DIST_OBJECT}"`,
                    "cache-control": "no-store",
                    etag: object.httpEtag,
                },
            });
        }

        return env.ASSETS.fetch(request);
    },
};
