#!/bin/bash
set -eux
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y nginx
mkdir -p /var/www/soils/videos
chown -R www-data:www-data /var/www/soils
cat > /etc/nginx/sites-available/soils <<'NGINX'
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    root /var/www/soils;
    index index.html;
    server_name _;
    location / { try_files $uri $uri/ =404; }
    location /videos/ {
        add_header Accept-Ranges bytes;
        add_header Cache-Control "public, max-age=3600";
    }
}
NGINX
ln -sf /etc/nginx/sites-available/soils /etc/nginx/sites-enabled/soils
rm -f /etc/nginx/sites-enabled/default
echo '<html><body><h1>new-soils dashboard</h1><p>provisioning...</p></body></html>' > /var/www/soils/index.html
systemctl enable nginx
systemctl restart nginx
