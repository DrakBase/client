# Image de la stack de démonstration : Alpine, la version et un service qui attend. Au démarrage,
# il affiche le réglage MESSAGE (stack/.env.example) et le fichier monté (stack/files/). Le
# contrôle de santé ne passe qu'une fois le démarrage fini, ce que l'agent attend après chaque
# mise à jour.
FROM alpine:3.24
COPY version.txt /etc/demo/version.txt
HEALTHCHECK --interval=2s --timeout=2s --start-period=2s --retries=5 CMD test -f /run/demo/ready
CMD ["sh", "-c", "mkdir -p /run/demo && echo \"demo $(cat /etc/demo/version.txt) : ${MESSAGE:-Bonjour} ($(cat /etc/demo/message.txt))\" | tee /data/last-start.txt && touch /run/demo/ready && exec sleep infinity"]
