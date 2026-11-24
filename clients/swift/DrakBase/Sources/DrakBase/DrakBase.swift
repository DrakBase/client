// Module unique que l'app importe (`import DrakBase`) : il réexporte l'updater, son interface et la
// vérification des signatures. Les modules internes gardent leur nom ; l'app n'a pas à les connaître.
@_exported import AppsMarketCore
@_exported import AppsMarketCrypto
@_exported import AppsMarketUI
